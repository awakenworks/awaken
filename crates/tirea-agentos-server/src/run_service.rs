use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use tirea_contract::storage::{
    RunOrigin, RunQuery, RunRecord, RunRecordStatus, RunStore, RunStoreError,
};
use tirea_contract::{AgentEvent, TerminationReason, Transcoder};
use tokio::sync::mpsc;

fn now_unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis().min(u128::from(u64::MAX)) as u64)
}

/// Durable run projection service.
pub struct RunService {
    store: Arc<dyn RunStore>,
}

impl RunService {
    pub fn new(store: Arc<dyn RunStore>) -> Self {
        Self { store }
    }

    pub async fn begin_intent(
        &self,
        run_id: &str,
        thread_id: &str,
        origin: RunOrigin,
        parent_run_id: Option<String>,
        parent_thread_id: Option<String>,
    ) -> Result<(), RunStoreError> {
        let now = now_unix_millis();
        let mut record = match self.store.load_run(run_id).await? {
            Some(existing) => existing,
            None => RunRecord::new(run_id, thread_id, origin, RunRecordStatus::Submitted, now),
        };
        if record.parent_run_id.is_none() {
            record.parent_run_id = parent_run_id;
        }
        if record.parent_thread_id.is_none() {
            record.parent_thread_id = parent_thread_id;
        }
        record.thread_id = thread_id.to_string();
        record.origin = origin;
        record.status = RunRecordStatus::Submitted;
        record.updated_at = now;
        self.store.upsert_run(&record).await
    }

    pub async fn apply_event(
        &self,
        run_id: &str,
        thread_id: &str,
        origin: RunOrigin,
        event: &AgentEvent,
    ) -> Result<(), RunStoreError> {
        let now = now_unix_millis();
        let mut record = match self.store.load_run(run_id).await? {
            Some(existing) => existing,
            None => RunRecord::new(run_id, thread_id, origin, RunRecordStatus::Submitted, now),
        };

        record.thread_id = thread_id.to_string();
        record.origin = origin;

        match event {
            AgentEvent::RunStart { parent_run_id, .. } => {
                if record.parent_run_id.is_none() {
                    record.parent_run_id = parent_run_id.clone();
                }
                if record.parent_thread_id.is_none() {
                    if let Some(parent_run_id) = parent_run_id.as_deref() {
                        record.parent_thread_id =
                            self.store.resolve_thread_id(parent_run_id).await?;
                    }
                }
                record.status = RunRecordStatus::Working;
                record.termination_code = None;
                record.termination_detail = None;
            }
            AgentEvent::RunFinish { termination, .. } => {
                let (status, code, detail) = map_termination(termination);
                record.status = status;
                record.termination_code = code;
                record.termination_detail = detail;
            }
            AgentEvent::Error { message, code } => {
                record.status = RunRecordStatus::Failed;
                record.termination_code = code.clone().or(Some("error".to_string()));
                record.termination_detail = Some(message.clone());
            }
            _ => {}
        }

        record.updated_at = now;
        self.store.upsert_run(&record).await
    }

    pub async fn get_run(&self, run_id: &str) -> Result<Option<RunRecord>, RunStoreError> {
        self.store.load_run(run_id).await
    }

    pub async fn list_runs(
        &self,
        query: &RunQuery,
    ) -> Result<tirea_contract::storage::RunPage, RunStoreError> {
        self.store.list_runs(query).await
    }

    pub async fn resolve_thread_id(&self, run_id: &str) -> Result<Option<String>, RunStoreError> {
        self.store.resolve_thread_id(run_id).await
    }
}

fn map_termination(
    termination: &TerminationReason,
) -> (RunRecordStatus, Option<String>, Option<String>) {
    match termination {
        TerminationReason::NaturalEnd | TerminationReason::BehaviorRequested => {
            (RunRecordStatus::Completed, None, None)
        }
        TerminationReason::Suspended => (
            RunRecordStatus::InputRequired,
            Some("input_required".to_string()),
            None,
        ),
        TerminationReason::Cancelled => (
            RunRecordStatus::Canceled,
            Some("cancelled".to_string()),
            None,
        ),
        TerminationReason::Error(message) => (
            RunRecordStatus::Failed,
            Some("error".to_string()),
            Some(message.clone()),
        ),
        TerminationReason::Stopped(stopped) => {
            let code = stopped.code.trim().to_ascii_lowercase();
            if code == "auth_required" {
                (
                    RunRecordStatus::AuthRequired,
                    Some(stopped.code.clone()),
                    stopped.detail.clone(),
                )
            } else if code == "rejected" {
                (
                    RunRecordStatus::Rejected,
                    Some(stopped.code.clone()),
                    stopped.detail.clone(),
                )
            } else if code == "cancelled" || code == "canceled" {
                (
                    RunRecordStatus::Canceled,
                    Some(stopped.code.clone()),
                    stopped.detail.clone(),
                )
            } else {
                (
                    RunRecordStatus::Completed,
                    Some(stopped.code.clone()),
                    stopped.detail.clone(),
                )
            }
        }
    }
}

/// Wrapper transcoder that mirrors incoming `AgentEvent`s into `RunService`.
pub struct RunTrackingTranscoder<E>
where
    E: Transcoder<Input = AgentEvent>,
{
    inner: E,
    tracker: RunEventTracker,
    update_tx: Option<mpsc::UnboundedSender<RunEventUpdate>>,
}

#[derive(Clone)]
struct TrackedRun {
    run_id: String,
    thread_id: String,
    origin: RunOrigin,
}

impl TrackedRun {
    fn new(run_id: impl Into<String>, thread_id: impl Into<String>, origin: RunOrigin) -> Self {
        Self {
            run_id: run_id.into(),
            thread_id: thread_id.into(),
            origin,
        }
    }
}

struct RunEventTracker {
    root: TrackedRun,
    stack: Vec<TrackedRun>,
    known: HashMap<String, TrackedRun>,
}

impl RunEventTracker {
    fn new(root: TrackedRun) -> Self {
        let mut known = HashMap::new();
        known.insert(root.run_id.clone(), root.clone());
        Self {
            stack: vec![root.clone()],
            known,
            root,
        }
    }

    fn context_for(&mut self, event: &AgentEvent) -> Option<TrackedRun> {
        match event {
            AgentEvent::RunStart {
                thread_id,
                run_id,
                parent_run_id,
            } => {
                let origin = if parent_run_id.is_some() {
                    RunOrigin::Subagent
                } else {
                    self.root.origin
                };
                let tracked = TrackedRun::new(run_id.clone(), thread_id.clone(), origin);
                self.known.insert(run_id.clone(), tracked.clone());
                self.retain_without(run_id);
                self.stack.push(tracked.clone());
                Some(tracked)
            }
            AgentEvent::RunFinish {
                thread_id, run_id, ..
            } => {
                let tracked = self.known.get(run_id).cloned().unwrap_or_else(|| {
                    if run_id == &self.root.run_id {
                        self.root.clone()
                    } else {
                        TrackedRun::new(run_id.clone(), thread_id.clone(), RunOrigin::Subagent)
                    }
                });
                self.known.remove(run_id);
                self.retain_without(run_id);
                if self.stack.is_empty() {
                    self.stack.push(self.root.clone());
                }
                Some(tracked)
            }
            AgentEvent::Error { .. } => Some(
                self.stack
                    .last()
                    .cloned()
                    .unwrap_or_else(|| self.root.clone()),
            ),
            _ => None,
        }
    }

    fn retain_without(&mut self, run_id: &str) {
        if let Some(pos) = self
            .stack
            .iter()
            .rposition(|candidate| candidate.run_id == run_id)
        {
            self.stack.remove(pos);
        }
    }
}

struct RunEventUpdate {
    event: AgentEvent,
    run_id: String,
    thread_id: String,
    origin: RunOrigin,
}

impl<E> RunTrackingTranscoder<E>
where
    E: Transcoder<Input = AgentEvent>,
{
    pub fn new(
        inner: E,
        run_id: impl Into<String>,
        thread_id: impl Into<String>,
        origin: RunOrigin,
        service: Option<Arc<RunService>>,
    ) -> Self {
        let root = TrackedRun::new(run_id, thread_id, origin);
        let tracker = RunEventTracker::new(root);
        let update_tx = service.map(|service| {
            let (tx, mut rx) = mpsc::unbounded_channel::<RunEventUpdate>();
            tokio::spawn(async move {
                while let Some(update) = rx.recv().await {
                    let _ = service
                        .apply_event(
                            &update.run_id,
                            &update.thread_id,
                            update.origin,
                            &update.event,
                        )
                        .await;
                }
            });
            tx
        });
        Self {
            inner,
            tracker,
            update_tx,
        }
    }
}

impl<E> Transcoder for RunTrackingTranscoder<E>
where
    E: Transcoder<Input = AgentEvent>,
{
    type Input = AgentEvent;
    type Output = E::Output;

    fn prologue(&mut self) -> Vec<Self::Output> {
        self.inner.prologue()
    }

    fn transcode(&mut self, item: &Self::Input) -> Vec<Self::Output> {
        if let Some(ctx) = self.tracker.context_for(item) {
            if let Some(tx) = self.update_tx.as_ref() {
                let _ = tx.send(RunEventUpdate {
                    event: item.clone(),
                    run_id: ctx.run_id,
                    thread_id: ctx.thread_id,
                    origin: ctx.origin,
                });
            }
        }
        self.inner.transcode(item)
    }

    fn epilogue(&mut self) -> Vec<Self::Output> {
        self.inner.epilogue()
    }
}

static RUN_SERVICE: OnceLock<Arc<RunService>> = OnceLock::new();

pub fn init_run_service(store: Arc<dyn RunStore>) -> Result<(), &'static str> {
    RUN_SERVICE
        .set(Arc::new(RunService::new(store)))
        .map_err(|_| "run service already initialized")
}

pub fn global_run_service() -> Option<Arc<RunService>> {
    RUN_SERVICE.get().cloned()
}

pub fn origin_from_protocol(protocol_label: &str) -> RunOrigin {
    match protocol_label {
        "ag_ui" | "ag-ui" | "agui" => RunOrigin::AgUi,
        "ai_sdk" | "ai-sdk" | "aisdk" => RunOrigin::AiSdk,
        "a2a" => RunOrigin::A2a,
        _ => RunOrigin::User,
    }
}

pub fn wrap_with_run_tracking<E>(
    inner: E,
    run_id: impl Into<String>,
    thread_id: impl Into<String>,
    protocol_label: &str,
) -> RunTrackingTranscoder<E>
where
    E: Transcoder<Input = AgentEvent>,
{
    RunTrackingTranscoder::new(
        inner,
        run_id,
        thread_id,
        origin_from_protocol(protocol_label),
        global_run_service(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tirea_contract::storage::{RunOrigin, RunRecordStatus};
    use tirea_store_adapters::MemoryRunStore;

    #[tokio::test]
    async fn begin_and_finish_run_records_status() {
        let store = Arc::new(MemoryRunStore::new());
        let svc = RunService::new(store.clone());

        svc.begin_intent("run-1", "thread-1", RunOrigin::AgUi, None, None)
            .await
            .expect("begin intent should succeed");
        let started = svc
            .get_run("run-1")
            .await
            .expect("query run")
            .expect("run-1 exists");
        assert_eq!(started.status, RunRecordStatus::Submitted);

        svc.apply_event(
            "run-1",
            "thread-1",
            RunOrigin::AgUi,
            &AgentEvent::RunFinish {
                thread_id: "thread-1".to_string(),
                run_id: "run-1".to_string(),
                result: None,
                termination: TerminationReason::NaturalEnd,
            },
        )
        .await
        .expect("apply finish should succeed");
        let completed = svc
            .get_run("run-1")
            .await
            .expect("query run")
            .expect("run-1 exists");
        assert_eq!(completed.status, RunRecordStatus::Completed);
    }

    #[tokio::test]
    async fn subagent_run_start_sets_origin_and_parent_thread() {
        let store = Arc::new(MemoryRunStore::new());
        let svc = RunService::new(store.clone());

        svc.begin_intent("run-root", "thread-root", RunOrigin::AgUi, None, None)
            .await
            .expect("begin root");
        svc.apply_event(
            "run-child",
            "thread-child",
            RunOrigin::Subagent,
            &AgentEvent::RunStart {
                thread_id: "thread-child".to_string(),
                run_id: "run-child".to_string(),
                parent_run_id: Some("run-root".to_string()),
            },
        )
        .await
        .expect("apply child start");

        let child = svc
            .get_run("run-child")
            .await
            .expect("query child")
            .expect("child exists");
        assert_eq!(child.origin, RunOrigin::Subagent);
        assert_eq!(child.parent_run_id.as_deref(), Some("run-root"));
        assert_eq!(child.parent_thread_id.as_deref(), Some("thread-root"));
        assert_eq!(child.status, RunRecordStatus::Working);
    }

    #[test]
    fn stopped_auth_maps_to_auth_required() {
        let termination = TerminationReason::stopped("auth_required");
        let (status, code, _) = map_termination(&termination);
        assert_eq!(status, RunRecordStatus::AuthRequired);
        assert_eq!(code.as_deref(), Some("auth_required"));
    }
}
