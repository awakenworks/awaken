pub mod resources;

use awaken_session_contract::SessionId;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use awaken_session_contract::{
    IdempotencyRecord, ManagedLifecycleFact, PersistedSession, SessionIdempotencyReceipt,
    SessionMutation, SessionMutationPayload, SessionMutationResult, SessionRepository,
    SessionRepositoryError,
};
use awaken_session_store::SqliteManagedSessionRepository;
use axum::http::StatusCode;

/// One HTTP exchange whose body cannot be read without naming the status it
/// came with.
///
/// The `(StatusCode, T)` tuple this replaces could be destructured as
/// `let (_, body) = ...`, and that is how a route which had begun answering
/// 404 went unnoticed: the test read the empty body, then waited for work the
/// failed request never scheduled, and parked its runtime forever. There is
/// no accessor here that yields the body alone, so that shape no longer
/// compiles.
#[allow(dead_code)] // This shared module is compiled independently by each integration binary.
#[must_use]
pub struct Call<T = serde_json::Value> {
    status: StatusCode,
    body: T,
}

#[allow(dead_code)] // This shared module is compiled independently by each integration binary.
impl<T: std::fmt::Debug> Call<T> {
    pub fn new(status: StatusCode, body: T) -> Self {
        Self { status, body }
    }

    /// Take the body, stating the status it must have arrived with.
    pub fn expect(self, status: StatusCode) -> T {
        assert_eq!(
            self.status, status,
            "unexpected status; body: {:?}",
            self.body
        );
        self.body
    }

    /// Take the body, naming the status and the decision-table rule the
    /// expectation belongs to. The body is already in the panic message, so
    /// the rule is all this needs to add.
    pub fn expect_for(self, status: StatusCode, rule: &str) -> T {
        assert_eq!(self.status, status, "{rule}; body: {:?}", self.body);
        self.body
    }

    /// Rewrite the body while the status rides along, so one helper can be
    /// built out of another without either of them handing the body over
    /// unqualified.
    pub fn map<U>(self, project: impl FnOnce(T) -> U) -> Call<U> {
        Call {
            status: self.status,
            body: project(self.body),
        }
    }

    /// Take the status when the body is not part of the assertion.
    pub fn status(self) -> StatusCode {
        self.status
    }
}

fn sdk_shape_names(shape: &serde_json::Value, key: &str, path: &str) -> BTreeSet<String> {
    shape[key]
        .as_array()
        .unwrap_or_else(|| panic!("current SDK {path} {key} properties are generated"))
        .iter()
        .map(|value| {
            value
                .as_str()
                .unwrap_or_else(|| panic!("current SDK {path} {key} property is a string"))
                .to_owned()
        })
        .collect()
}

fn assert_sdk_object_shape(value: &serde_json::Value, shape: &serde_json::Value, path: &str) {
    let required = sdk_shape_names(shape, "required", path);
    let optional = sdk_shape_names(shape, "optional", path);
    let object = value
        .as_object()
        .unwrap_or_else(|| panic!("{path} response is an object"));
    let actual: BTreeSet<String> = object.keys().cloned().collect();
    let allowed: BTreeSet<String> = required.union(&optional).cloned().collect();
    let missing: BTreeSet<_> = required.difference(&actual).cloned().collect();
    let unknown: BTreeSet<_> = actual.difference(&allowed).cloned().collect();
    assert!(
        missing.is_empty(),
        "missing current SDK {path} fields: {missing:?}"
    );
    assert!(
        unknown.is_empty(),
        "non-SDK {path} fields leaked: {unknown:?}"
    );

    let Some(nested) = shape.get("nested") else {
        return;
    };
    for (field, nested_shape) in nested
        .as_object()
        .unwrap_or_else(|| panic!("current SDK {path} nested shapes are generated"))
    {
        if let Some(nested_value) = object.get(field) {
            assert_sdk_object_shape(nested_value, nested_shape, &format!("{path}.{field}"));
        }
    }
}

/// Assert one serialized Session recursively against the current official SDK
/// interfaces extracted by `@awaken/managed-sdk-oracle`. Optional upstream
/// properties may be absent, but adapter-private properties and missing required
/// properties both fail closed at every generated object boundary.
#[allow(dead_code)] // This shared module is compiled independently by each integration binary.
pub fn assert_current_sdk_session_shape(session: &serde_json::Value) {
    let oracle: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../../contracts/anthropic-managed/upstream-oracle.generated.json"
    ))
    .expect("generated Managed SDK oracle is valid JSON");
    let shape = &oracle["current"]["wire_contract"]["session"];
    assert_sdk_object_shape(session, shape, "Session");
}

/// Assert one request Resource variant against the exact current SDK interface
/// selected by its generated discriminator. The generated oracle owns every
/// allowed/required field; Rust tests do not maintain a parallel field list.
#[allow(dead_code)]
pub fn assert_current_sdk_resource_param_shape(resource: &serde_json::Value) {
    let oracle: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../../contracts/anthropic-managed/upstream-oracle.generated.json"
    ))
    .expect("generated Managed SDK oracle is valid JSON");
    let discriminator = resource["type"]
        .as_str()
        .expect("Managed Resource discriminator is a string");
    let shape = &oracle["current"]["wire_contract"]["resource_params"][discriminator];
    assert!(
        shape.is_object(),
        "current SDK Resource variant is generated"
    );
    assert_sdk_object_shape(resource, shape, &format!("ResourceParam.{discriminator}"));
}

/// Advance retained Session Event commands through the production-owned
/// [`awaken_session_application::SessionApplication`] driver.
///
/// Integration tests call this instead of teaching an HTTP request or Runtime
/// fake to execute queued User work synchronously. Product composition invokes
/// the same driver from its one lifecycle supervisor.
#[allow(dead_code)] // This shared module is compiled independently by each integration binary.
pub async fn drive_retained_session_events(
    state: &awaken_protocol_managed::ManagedState,
    session_id: &str,
) {
    // Product reconciliation enters this large Resource/realization/Runtime
    // graph from its process-owned supervisor task. Preserve that scheduler
    // boundary here: directly nesting the complete graph under a libtest future
    // can exceed the default worker stack and is not the deployed execution
    // shape. JoinSet keeps cancellation structural rather than detaching work.
    let application = state.session_application();
    let session_id = session_id.to_string();
    let mut task = tokio::task::JoinSet::new();
    task.spawn(async move {
        application
            .drive_session_event_batches(&SessionId::from(session_id), None)
            .await
    });
    match task.join_next().await {
        Some(Ok(Ok(()))) => {}
        Some(Ok(Err(error))) => {
            panic!("drive retained Session Event batch through the canonical application: {error}")
        }
        Some(Err(error)) => panic!("retained Session Event supervisor task failed: {error}"),
        None => panic!("retained Session Event supervisor task disappeared"),
    }
}

/// Deliver a simulated applied Dispatch through the production Session observer.
#[allow(dead_code)]
pub async fn drive_completed_user_dispatches(
    state: &awaken_protocol_managed::ManagedState,
    session_id: &str,
    completions: &Mutex<Vec<awaken_session_contract::SessionAgentBoundaryCommand>>,
) {
    // Run only the canonical supervisor, then deliver exact activation facts.
    // No scan of active epochs can substitute for a simulated Applied receipt.
    drive_retained_session_events(state, session_id).await;
    let boundaries = std::mem::take(&mut *completions.lock().unwrap());
    for boundary in boundaries {
        assert_eq!(boundary.session_id, session_id, "fixture exact Session");
        finalize_applied_user_dispatch(state, boundary).await;
    }
}

/// Deliver a simulated applied Dispatch through the production Session observer.
#[allow(dead_code)]
pub async fn finalize_applied_user_dispatch(
    state: &awaken_protocol_managed::ManagedState,
    boundary: awaken_session_contract::SessionAgentBoundaryCommand,
) {
    // C1 exact activation coordinates retained by the fake; C2 committed Run;
    // C3 simulated Dispatch CAS Applied. C1+C2+C3 => the production callback
    // records processed and removes only that activity; duplicate callback is
    // a no-op. Calling this without C3 is forbidden: Thread completion alone
    // is not a Dispatch receipt. Production Host/queue tests own actual CAS.
    use awaken_session_contract::SessionAgentCoordination;
    let application = state.session_application();
    application
        .settle_session_agent_boundary(boundary.clone())
        .await
        .expect("fixture pre-Dispatch boundary");
    for _ in 0..2 {
        application
            .finalize_session_agent_boundary(boundary.clone())
            .await
            .expect("fixture applied Dispatch boundary");
    }
}

/// Shared integration-test decorator for deterministic root-CAS races.
///
/// It delegates every real read/write to the SQLite adapter; only selected
/// commit call numbers return `Conflict` or `Unavailable`. A selected conflict
/// may first commit another valid aggregate mutation to prove that callers
/// rebase only the sub-aggregate they own and never overwrite a concurrent fact.
pub struct ScheduledConflictRepository {
    inner: Arc<SqliteManagedSessionRepository>,
    commit_calls: AtomicUsize,
    conflicts: Mutex<BTreeSet<usize>>,
    unavailable: Mutex<BTreeSet<usize>>,
    resource_changes: Mutex<BTreeSet<usize>>,
    metadata_changes: Mutex<BTreeMap<usize, (String, String)>>,
}

#[allow(dead_code)]
impl ScheduledConflictRepository {
    pub fn new(inner: Arc<SqliteManagedSessionRepository>) -> Self {
        Self {
            inner,
            commit_calls: AtomicUsize::new(0),
            conflicts: Mutex::new(BTreeSet::new()),
            unavailable: Mutex::new(BTreeSet::new()),
            resource_changes: Mutex::new(BTreeSet::new()),
            metadata_changes: Mutex::new(BTreeMap::new()),
        }
    }

    /// Make the `offset`th subsequent commit report a CAS conflict.
    pub fn conflict_on_next(&self, offset: usize) {
        let call = self.commit_calls.load(Ordering::SeqCst) + offset;
        self.conflicts.lock().unwrap().insert(call);
    }

    /// Make several subsequent commits report CAS conflicts.
    pub fn conflicts_on_next(&self, offsets: &[usize]) {
        for offset in offsets {
            self.conflict_on_next(*offset);
        }
    }

    /// Make the `offset`th subsequent commit fail before the Session mutation.
    pub fn unavailable_on_next(&self, offset: usize) {
        let call = self.commit_calls.load(Ordering::SeqCst) + offset;
        self.unavailable.lock().unwrap().insert(call);
    }

    /// Before reporting the selected conflict, commit another valid Resource
    /// preparation so the loser observes a changed Resource sub-aggregate.
    pub fn resource_change_on_next(&self, offset: usize) {
        let call = self.commit_calls.load(Ordering::SeqCst) + offset;
        self.conflicts.lock().unwrap().insert(call);
        self.resource_changes.lock().unwrap().insert(call);
    }

    /// Before reporting the selected conflict, commit an unrelated metadata
    /// fact. A retrying narrow command must preserve it.
    pub fn metadata_change_on_next(&self, offset: usize, key: &str, value: &str) {
        let call = self.commit_calls.load(Ordering::SeqCst) + offset;
        self.conflicts.lock().unwrap().insert(call);
        self.metadata_changes
            .lock()
            .unwrap()
            .insert(call, (key.to_string(), value.to_string()));
    }

    pub fn commit_call_count(&self) -> usize {
        self.commit_calls.load(Ordering::SeqCst)
    }
}

#[allow(dead_code)]
pub async fn replace_session_fixture(
    repo: &dyn awaken_session_contract::SessionStore,
    owner: &str,
    mut session: PersistedSession,
    key: &str,
) -> PersistedSession {
    session.revision = repo.get(&session.session_id).await.unwrap().revision;
    let payload = SessionMutationPayload::Replace(session.clone());
    let payload_hash = payload.stable_hash();
    let result = repo
        .commit(
            owner,
            SessionMutation {
                expected_revision: session.revision,
                idempotency: IdempotencyRecord {
                    key: key.into(),
                    payload_hash,
                },
                payload,
                lifecycle_facts: Vec::new(),
            },
        )
        .await
        .unwrap();
    session.revision = match result {
        SessionMutationResult::Applied { new_revision }
        | SessionMutationResult::Replayed { new_revision } => new_revision,
        other => panic!("replace Session fixture failed: {other:?}"),
    };
    session
}

#[async_trait::async_trait]
impl SessionRepository for ScheduledConflictRepository {
    async fn create(
        &self,
        owner_scope: &str,
        session: PersistedSession,
        idempotency: IdempotencyRecord,
        lifecycle_facts: Vec<ManagedLifecycleFact>,
    ) -> Result<awaken_session_contract::SessionCreateResult, SessionRepositoryError> {
        self.inner
            .create(owner_scope, session, idempotency, lifecycle_facts)
            .await
    }

    async fn replay_create(
        &self,
        owner_scope: &str,
        session_id: &SessionId,
        idempotency: &IdempotencyRecord,
    ) -> Result<Option<PersistedSession>, SessionRepositoryError> {
        self.inner
            .replay_create(owner_scope, session_id, idempotency)
            .await
    }

    async fn commit(
        &self,
        owner_scope: &str,
        mutation: SessionMutation,
    ) -> Result<SessionMutationResult, SessionRepositoryError> {
        let call = self.commit_calls.fetch_add(1, Ordering::SeqCst) + 1;
        if self.unavailable.lock().unwrap().contains(&call) {
            return Err(SessionRepositoryError::Unavailable(
                "injected root-CAS outage".into(),
            ));
        }
        if self.conflicts.lock().unwrap().contains(&call) {
            let metadata_change = { self.metadata_changes.lock().unwrap().get(&call).cloned() };
            if let Some((key, value)) = metadata_change {
                let mut concurrent = self
                    .inner
                    .get(mutation.payload.session_id())
                    .await
                    .expect("concurrent metadata Session exists");
                concurrent.metadata.insert(key, value);
                let payload = SessionMutationPayload::Replace(concurrent.clone());
                let payload_hash = payload.stable_hash();
                let concurrent_mutation = SessionMutation {
                    expected_revision: concurrent.revision,
                    idempotency: IdempotencyRecord {
                        key: format!("test:concurrent-metadata:{call}:{payload_hash}"),
                        payload_hash,
                    },
                    payload,
                    lifecycle_facts: Vec::new(),
                };
                assert!(matches!(
                    self.inner.commit(owner_scope, concurrent_mutation).await?,
                    SessionMutationResult::Applied { .. }
                ));
            }
            if self.resource_changes.lock().unwrap().contains(&call) {
                let mut concurrent = self
                    .inner
                    .get(mutation.payload.session_id())
                    .await
                    .expect("concurrent Resource Session exists");
                let session_id = concurrent.session_id.clone();
                concurrent
                    .resources
                    .prepare(session_id.as_str(), concurrent.resources.active.clone())
                    .expect("concurrent Resource intent is valid");
                concurrent
                    .resources
                    .start_attempt()
                    .expect("concurrent Resource attempt is valid");
                let payload = SessionMutationPayload::Replace(concurrent.clone());
                let payload_hash = payload.stable_hash();
                let concurrent_mutation = SessionMutation {
                    expected_revision: concurrent.revision,
                    idempotency: IdempotencyRecord {
                        key: format!("test:concurrent-resource:{call}:{payload_hash}"),
                        payload_hash,
                    },
                    payload,
                    lifecycle_facts: Vec::new(),
                };
                assert!(matches!(
                    self.inner.commit(owner_scope, concurrent_mutation).await?,
                    SessionMutationResult::Applied { .. }
                ));
            }
            let current_revision = self
                .inner
                .get(mutation.payload.session_id())
                .await
                .map(|session| session.revision)
                .or_else(|error| match error {
                    SessionRepositoryError::NotFound => Ok(Default::default()),
                    error => Err(error),
                })?;
            return Ok(SessionMutationResult::Conflict { current_revision });
        }
        self.inner.commit(owner_scope, mutation).await
    }

    async fn get(
        &self,
        session_id: &SessionId,
    ) -> Result<PersistedSession, SessionRepositoryError> {
        self.inner.get(session_id).await
    }

    async fn idempotency_receipt(
        &self,
        session_id: &SessionId,
        key: &str,
    ) -> Result<Option<SessionIdempotencyReceipt>, SessionRepositoryError> {
        self.inner.idempotency_receipt(session_id, key).await
    }

    async fn owner(&self, session_id: &SessionId) -> Result<String, SessionRepositoryError> {
        self.inner.owner(session_id).await
    }

    async fn scan(
        &self,
        query: awaken_session_contract::SessionScanQuery<'_>,
    ) -> Result<awaken_session_contract::SessionScan, awaken_session_contract::SessionRepositoryError>
    {
        self.inner.scan(query).await
    }

    async fn deleted_event_admissions(
        &self,
        owner_scope: &str,
        session_id: &awaken_session_contract::SessionId,
    ) -> Result<
        Option<awaken_session_contract::DeletedSessionEventAdmissions>,
        awaken_session_contract::SessionRepositoryError,
    > {
        self.inner
            .deleted_event_admissions(owner_scope, session_id)
            .await
    }

    async fn count_environment_phase(
        &self,
        phase: awaken_session_contract::SessionEnvironmentPhase,
    ) -> Result<u64, awaken_session_contract::SessionRepositoryError> {
        self.inner.count_environment_phase(phase).await
    }
}

#[async_trait::async_trait]
impl awaken_session_contract::LifecycleOutbox for ScheduledConflictRepository {
    async fn append_lifecycle(
        &self,
        fact: ManagedLifecycleFact,
    ) -> Result<(), SessionRepositoryError> {
        self.inner.append_lifecycle(fact).await
    }

    async fn pending_lifecycle(&self) -> Result<Vec<ManagedLifecycleFact>, SessionRepositoryError> {
        self.inner.pending_lifecycle().await
    }

    async fn complete_lifecycle(&self, fact_id: &str) -> Result<(), SessionRepositoryError> {
        self.inner.complete_lifecycle(fact_id).await
    }
}

/// Mount a Managed router the way a real composition root does: the trusted
/// startup edge stamps the installation's Workspace on any request that did not
/// already carry one (`awaken_coordinator::workspace_path::with_platform_workspace`
/// in the CLI; the IAM edge otherwise). Handlers are fail-closed on an absent
/// scope, so a test that drives the bare router without this layer is
/// deliberately exercising the unstamped-ingress refusal.
#[allow(dead_code)]
pub fn edge_stamped(inner: axum::Router) -> axum::Router {
    inner.layer(axum::middleware::from_fn(
        |mut request: axum::extract::Request, next: axum::middleware::Next| async move {
            if request
                .extensions()
                .get::<awaken_tenancy::WorkspaceScope>()
                .is_none()
            {
                request
                    .extensions_mut()
                    .insert(awaken_tenancy::WorkspaceScope(
                        awaken_tenancy::DEFAULT_WORKSPACE_ID.to_string(),
                    ));
            }
            next.run(request).await
        },
    ))
}
