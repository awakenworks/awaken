pub mod resources;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use awaken_session_contract::{
    IdempotencyRecord, ManagedLifecycleFact, ManagedSessionRepository, PersistedSession,
    SessionIdempotencyReceipt, SessionMutation, SessionMutationPayload, SessionMutationResult,
    SessionRepositoryError,
};
use awaken_session_store::SqliteManagedSessionRepository;

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
            .drive_session_event_batches(&session_id, None)
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
    repo: &dyn ManagedSessionRepository,
    owner: &str,
    mut session: PersistedSession,
    key: &str,
) -> PersistedSession {
    session.revision = repo.get(&session.session_id).await.unwrap().revision;
    let payload = SessionMutationPayload::Replace(session.clone());
    let payload_hash = payload.stable_hash();
    let result = repo
        .commit_mutation(
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
impl ManagedSessionRepository for ScheduledConflictRepository {
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
        session_id: &str,
        idempotency: &IdempotencyRecord,
    ) -> Result<Option<PersistedSession>, SessionRepositoryError> {
        self.inner
            .replay_create(owner_scope, session_id, idempotency)
            .await
    }

    async fn commit_mutation(
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
                    self.inner
                        .commit_mutation(owner_scope, concurrent_mutation)
                        .await?,
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
                    .prepare(&session_id, concurrent.resources.active.clone())
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
                    self.inner
                        .commit_mutation(owner_scope, concurrent_mutation)
                        .await?,
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
        self.inner.commit_mutation(owner_scope, mutation).await
    }

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

    async fn get(&self, session_id: &str) -> Result<PersistedSession, SessionRepositoryError> {
        self.inner.get(session_id).await
    }

    async fn reconcilable_sessions_page(
        &self,
        after: Option<&awaken_session_contract::SessionRecoveryCursor>,
    ) -> Result<awaken_session_contract::SessionRecoveryScan, SessionRepositoryError> {
        self.inner.reconcilable_sessions_page(after).await
    }

    async fn sessions_referencing_credential_source(
        &self,
        workspace_id: &str,
        source_id: &awaken_credential_contract::CredentialSourceId,
    ) -> Result<Vec<PersistedSession>, SessionRepositoryError> {
        self.inner
            .sessions_referencing_credential_source(workspace_id, source_id)
            .await
    }

    async fn idempotency_receipt(
        &self,
        session_id: &str,
        key: &str,
    ) -> Result<Option<SessionIdempotencyReceipt>, SessionRepositoryError> {
        self.inner.idempotency_receipt(session_id, key).await
    }

    async fn owner(&self, session_id: &str) -> Result<String, SessionRepositoryError> {
        self.inner.owner(session_id).await
    }
}
