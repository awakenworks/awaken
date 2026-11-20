// @formal surface requirement=store.backends.schema_and_adapter_conformance
// @formal reason: Adapts tenant-scoped Session root and event operations to the object repository.
//! Session repository over one engine-owned conditional Session document.
//! Domain admission is shared with SQL. The object engine owns routing and CAS.
use super::{
    BTreeMap, EncodedSessionRow, IdempotencyRecord, LifecycleOutbox, ManagedLifecycleFact,
    MissingCreateReceipt, PersistedSession, RECOVERY_BATCH_SIZE, SESSION_CREATE_REVISION,
    ScopedPersistedSession, SessionCommandReceiptKind, SessionCreateResult,
    SessionEnvironmentPhase, SessionIdempotencyReceipt, SessionMutation, SessionMutationPayload,
    SessionMutationResult, SessionReconciliation, SessionRecoveryQuarantine, SessionRepository,
    SessionRepositoryConflict, SessionRepositoryError, SessionRepositoryReads, SessionRevision,
    SessionScan, TransactionalSessionIdentity, aggregate_str, async_trait, classify_create_replay,
    classify_mutation_replay, content_visibility, corrupt, creation_intents, db_revision, decode,
    event_admissions, referenced_mcp_credential_source_ids, referenced_vault_ids,
    validate_current_mutation, validate_session_create_command,
};
use awaken_session_contract::{
    DeletedSessionEventAdmissions, PreparedParticipantIntent, PreparedParticipantIntentClaim,
    PreparedParticipantIntentPhase, PreparedParticipantIntentRecord, PreparedParticipantIntents,
    SessionConditionalPublication, SessionScanCursor, SessionScanQuery, SessionTombstone,
};
use awaken_session_types::SessionId;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use awaken_object_storage::{SessionObject, SessionObjectStore, SessionObjectUpdate};
#[derive(Clone, Serialize, Deserialize)]
struct Intent {
    repositories: Vec<String>,
    phase: String,
    expires: u64,
}
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(try_from = "u32", into = "u32")]
struct FormatVersion;
impl Default for FormatVersion {
    fn default() -> Self {
        Self
    }
}
impl TryFrom<u32> for FormatVersion {
    type Error = String;
    fn try_from(value: u32) -> Result<Self, String> {
        if value == 1 {
            Ok(Self)
        } else {
            Err("unsupported Session component format".into())
        }
    }
}
impl From<FormatVersion> for u32 {
    fn from(_: FormatVersion) -> Self {
        1
    }
}
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    #[serde(default)]
    format: FormatVersion,
    root: Option<String>,
    revision: u64,
    tombstone: Option<SessionTombstone>,
    deleted_admissions: Option<String>,
    private_content: bool,
    receipts: BTreeMap<String, SessionIdempotencyReceipt>,
    outbox: Vec<(ManagedLifecycleFact, bool)>,
    intent: Option<Intent>,
    quarantined: Option<String>,
    reconcilable: bool,
}
impl State {
    fn decode(object: &SessionObject) -> Result<Self, SessionRepositoryError> {
        match object.payload.as_deref() {
            Some(payload) => serde_json::from_str(payload).map_err(corrupt),
            None => Ok(Self::default()),
        }
    }
    fn identity(
        &self,
        owner: &str,
    ) -> Result<TransactionalSessionIdentity, SessionRepositoryError> {
        if self.root.is_some() && self.tombstone.is_some() {
            return Err(corrupt("mixed live/tombstone identity"));
        }
        Ok(match (&self.root, &self.tombstone) {
            (Some(root), None) => TransactionalSessionIdentity::Live {
                owner_scope: owner.into(),
                row: EncodedSessionRow {
                    aggregate_json: root.clone(),
                    revision: db_revision(SessionRevision(self.revision))?,
                },
            },
            (None, Some(t)) => TransactionalSessionIdentity::Tombstone {
                owner_scope: owner.into(),
                revision: t.deleted_revision,
            },
            _ => TransactionalSessionIdentity::Absent,
        })
    }
    fn session(&self, id: &str) -> Result<PersistedSession, SessionRepositoryError> {
        let root = self.root.clone().ok_or(SessionRepositoryError::NotFound)?;
        let session = decode(EncodedSessionRow {
            aggregate_json: root,
            revision: db_revision(SessionRevision(self.revision))?,
        })
        .map_err(corrupt)?;
        if session.session_id.as_str() != id {
            return Err(corrupt("Session object identity mismatch"));
        }
        Ok(session)
    }
    fn append(&mut self, facts: impl IntoIterator<Item = ManagedLifecycleFact>) {
        for fact in facts {
            if !self.outbox.iter().any(|(old, _)| old.id == fact.id) {
                self.outbox.push((fact, false));
            }
        }
    }
    fn set_root(&mut self, session: &PersistedSession) -> Result<(), SessionRepositoryError> {
        self.root = Some(aggregate_str(session)?);
        self.revision = session.revision.0;
        self.reconcilable = session.needs_reconciliation();
        self.quarantined = None;
        Ok(())
    }
    fn intent(
        &self,
        object: &SessionObject,
    ) -> Result<Option<PreparedParticipantIntent>, SessionRepositoryError> {
        self.intent
            .as_ref()
            .map(|i| {
                Ok(PreparedParticipantIntent::new(
                    object.session_id.clone().into(),
                    object.owner_scope.clone(),
                    i.repositories.clone(),
                    creation_intents::decode_phase(&i.phase)?,
                    i.expires,
                ))
            })
            .transpose()
    }
}
/// Selected repository implementation; all writes publish through the supplied
/// engine rather than a SQL pool or a second local authority.
#[derive(Clone)]
pub struct ObjectManagedSessionRepository {
    store: ObjectPublication,
}
impl ObjectManagedSessionRepository {
    pub fn new(store: Arc<dyn SessionObjectStore>) -> Self {
        Self {
            store: ObjectPublication(store),
        }
    }
    async fn commit_root(
        &self,
        owner: &str,
        mutation: SessionMutation,
        condition: Option<&awaken_session_types::SessionPublicationCondition>,
    ) -> Result<SessionMutationResult, SessionRepositoryError> {
        let next = mutation
            .validate()
            .map_err(|e| SessionRepositoryError::InvalidMutation(e.to_string()))?;
        let id = mutation.payload.session_id().to_string();
        for _ in 0..32 {
            let Some(object) = self.store.load(&id).await? else {
                return Ok(SessionMutationResult::Conflict {
                    current_revision: SessionRevision(0),
                });
            };
            let mut state = State::decode(&object)?;
            if let Some(receipt) = state.receipts.get(&mutation.idempotency.key) {
                return classify_mutation_replay(
                    owner,
                    receipt,
                    &mutation,
                    next,
                    &state.identity(&object.owner_scope)?,
                );
            }
            let current = state
                .tombstone
                .as_ref()
                .map_or(SessionRevision(state.revision), |t| t.deleted_revision);
            if object.owner_scope != owner
                || state.root.is_none()
                || current != mutation.expected_revision
            {
                return Ok(SessionMutationResult::Conflict {
                    current_revision: current,
                });
            }
            let (mut admissions, private) = validate_current_mutation(
                &mutation,
                EncodedSessionRow {
                    aggregate_json: state.root.clone().ok_or(SessionRepositoryError::NotFound)?,
                    revision: db_revision(current)?,
                },
            )?;
            match &mutation.payload {
                SessionMutationPayload::Replace(replacement) => {
                    let mut root = replacement.clone();
                    root.revision = next;
                    state.set_root(&root)?;
                }
                SessionMutationPayload::Delete(tombstone) => {
                    let noops = state
                        .receipts
                        .iter()
                        .filter(|(_, r)| r.kind() == SessionCommandReceiptKind::EventNoOp)
                        .map(|(key, r)| {
                            Ok((
                                key.clone(),
                                r.payload_hash.clone(),
                                db_revision(r.committed_revision)?,
                            ))
                        })
                        .collect::<Result<Vec<_>, SessionRepositoryError>>()?;
                    if !noops.is_empty() {
                        admissions = Some(event_admissions::merge_noops(
                            admissions
                                .as_deref()
                                .ok_or_else(|| corrupt("missing delete admission evidence"))?,
                            current,
                            noops,
                        )?);
                    }
                    state.root = None;
                    state.revision = next.0;
                    state.tombstone = Some(tombstone.clone());
                    state.deleted_admissions = admissions;
                    state.private_content = private;
                    state.reconcilable = false;
                    state.quarantined = None;
                }
                SessionMutationPayload::EventNoOp { .. } => {}
            }
            let kind = mutation.receipt_kind();
            state.receipts.insert(
                mutation.idempotency.key.clone(),
                SessionIdempotencyReceipt::new(
                    mutation.idempotency.payload_hash.clone(),
                    next,
                    kind,
                ),
            );
            state.append(mutation.lifecycle_facts.clone());
            if self
                .persist_conditionally(Some(&object), owner, &id, &state, condition)
                .await?
            {
                return Ok(if kind == SessionCommandReceiptKind::EventNoOp {
                    SessionMutationResult::AcknowledgedNoOp {
                        observed_revision: next,
                    }
                } else {
                    SessionMutationResult::Applied { new_revision: next }
                });
            }
        }
        Err(busy())
    }
    async fn persist(
        &self,
        observed: Option<&SessionObject>,
        owner: &str,
        id: &str,
        state: &State,
    ) -> Result<bool, SessionRepositoryError> {
        self.persist_conditionally(observed, owner, id, state, None)
            .await
    }
    async fn persist_conditionally(
        &self,
        observed: Option<&SessionObject>,
        owner: &str,
        id: &str,
        state: &State,
        condition: Option<&awaken_session_types::SessionPublicationCondition>,
    ) -> Result<bool, SessionRepositoryError> {
        if observed.is_some_and(|object| object.owner_scope != owner) {
            return Err(SessionRepositoryError::NotApplied(
                "Session owner scope mismatch".into(),
            ));
        }
        let mut update =
            SessionObjectUpdate::snapshot(serde_json::to_string(state).map_err(corrupt)?);
        update.condition = condition.map(|condition| condition.token().to_owned());
        match observed {
            Some(object) => self.store.replace(object, &update).await,
            None => self.store.insert(owner, id, &update).await,
        }
    }
    async fn objects(&self) -> Result<Vec<SessionObject>, SessionRepositoryError> {
        let mut objects = self.store.list().await?;
        objects.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        for pair in objects.windows(2) {
            if pair[0].session_id == pair[1].session_id {
                return Err(corrupt("duplicate Session identity in object authority"));
            }
        }
        Ok(objects)
    }
}
fn busy() -> SessionRepositoryError {
    SessionRepositoryError::NotApplied("Session contention exceeded retry bound".into())
}
#[async_trait]
impl SessionRepositoryReads for ObjectManagedSessionRepository {
    async fn get(&self, id: &SessionId) -> Result<PersistedSession, SessionRepositoryError> {
        let object = self
            .store
            .load(id.as_str())
            .await?
            .ok_or(SessionRepositoryError::NotFound)?;
        State::decode(&object)?.session(id.as_str())
    }
    async fn owner(&self, id: &SessionId) -> Result<String, SessionRepositoryError> {
        let object = self
            .store
            .load(id.as_str())
            .await?
            .ok_or(SessionRepositoryError::NotFound)?;
        if State::decode(&object)?.root.is_none() {
            return Err(SessionRepositoryError::NotFound);
        }
        Ok(object.owner_scope)
    }
    async fn workspace_content_is_public(
        &self,
        owner: &str,
        id: &SessionId,
    ) -> Result<bool, SessionRepositoryError> {
        let Some(object) = self.store.load(id.as_str()).await? else {
            return Ok(false);
        };
        let state = State::decode(&object)?;
        let rows = if let Some(root) = state.root {
            vec![(
                object.owner_scope,
                db_revision(SessionRevision(state.revision))?,
                Some(root),
                None,
            )]
        } else if let Some(t) = state.tombstone {
            vec![(
                object.owner_scope,
                db_revision(t.deleted_revision)?,
                None,
                Some(i64::from(state.private_content)),
            )]
        } else {
            Vec::new()
        };
        content_visibility::workspace_content_is_public(owner, id.as_str(), rows)
    }
    async fn scan(
        &self,
        query: SessionScanQuery<'_>,
    ) -> Result<SessionScan, SessionRepositoryError> {
        let mut sessions = Vec::new();
        for object in self.objects().await? {
            let state = State::decode(&object)?;
            if state.root.is_none() {
                continue;
            }
            let session = state.session(&object.session_id)?;
            let matches = match query {
                SessionScanQuery::Workspace { workspace_id } => workspace_id == object.owner_scope,
                SessionScanQuery::WorkerRealizationOwner { worker_id } => session
                    .realization
                    .as_ref()
                    .is_some_and(|l| l.owner == worker_id),
                SessionScanQuery::VaultReferences {
                    workspace_id,
                    vault_id,
                } => {
                    workspace_id == object.owner_scope
                        && !session.is_terminal()
                        && referenced_vault_ids(&session).contains(vault_id)
                }
                SessionScanQuery::CredentialSourceReferences {
                    workspace_id,
                    source_id,
                } => {
                    workspace_id == object.owner_scope
                        && !session.is_terminal()
                        && referenced_mcp_credential_source_ids(&session).contains(&source_id.0)
                }
            };
            if matches {
                sessions.push(ScopedPersistedSession {
                    workspace_id: object.owner_scope,
                    session,
                });
            }
        }
        Ok(SessionScan {
            sessions,
            ..Default::default()
        })
    }
    async fn deleted_event_admissions(
        &self,
        owner: &str,
        id: &SessionId,
    ) -> Result<Option<DeletedSessionEventAdmissions>, SessionRepositoryError> {
        let Some(object) = self.store.load(id.as_str()).await? else {
            return Ok(None);
        };
        if object.owner_scope != owner {
            return Ok(None);
        }
        let state = State::decode(&object)?;
        state
            .tombstone
            .map(|t| {
                event_admissions::decode(db_revision(t.deleted_revision)?, state.deleted_admissions)
            })
            .transpose()
    }
}
#[async_trait]
impl SessionConditionalPublication for ObjectManagedSessionRepository {
    async fn commit_conditionally(
        &self,
        owner: &str,
        mutation: SessionMutation,
        condition: &awaken_session_types::SessionPublicationCondition,
    ) -> Result<SessionMutationResult, SessionRepositoryError> {
        self.commit_root(owner, mutation, Some(condition)).await
    }
    async fn validate_publication_condition(
        &self,
        id: &SessionId,
        condition: &awaken_session_types::SessionPublicationCondition,
        revision: Option<awaken_session_types::SessionRevision>,
    ) -> Result<bool, SessionRepositoryError> {
        if let Some(revision) = revision {
            let Some(observed) = self.store.load(id.as_str()).await? else {
                return Ok(false);
            };
            let current = State::decode(&observed)?.session(id.as_str())?;
            if current.revision != revision {
                return Ok(false);
            }
            return self
                .store
                .0
                .validate_condition(id.as_str(), condition.token(), Some(&observed.version))
                .await
                .map_err(publication_error);
        }
        self.store
            .0
            .validate_condition(id.as_str(), condition.token(), None)
            .await
            .map_err(publication_error)
    }
}

#[async_trait]
impl SessionRepository for ObjectManagedSessionRepository {
    async fn create(
        &self,
        owner: &str,
        mut session: PersistedSession,
        identity: IdempotencyRecord,
        facts: Vec<ManagedLifecycleFact>,
    ) -> Result<SessionCreateResult, SessionRepositoryError> {
        validate_session_create_command(owner, &session, &identity, &facts)?;
        let id = session.session_id.to_string();
        for _ in 0..32 {
            let observed = self.store.load(&id).await?;
            let mut state = observed
                .as_ref()
                .map(State::decode)
                .transpose()?
                .unwrap_or_default();
            let stored_owner = observed.as_ref().map_or(owner, |o| o.owner_scope.as_str());
            if let Some(replay) = classify_create_replay(
                owner,
                &id,
                &identity,
                state.receipts.get(&identity.key).cloned(),
                state.identity(stored_owner)?,
                MissingCreateReceipt::RejectOccupied,
            )? {
                return Ok(SessionCreateResult::Replayed(replay));
            }
            if stored_owner != owner {
                return Err(SessionRepositoryError::Conflict(
                    SessionRepositoryConflict::AlreadyExists,
                ));
            }
            if state
                .intent
                .as_ref()
                .is_some_and(|i| i.phase != creation_intents::PREPARING)
            {
                return Err(SessionRepositoryError::Conflict(
                    SessionRepositoryConflict::Tombstoned,
                ));
            }
            session.revision = SESSION_CREATE_REVISION;
            // Same complete-root codec and validation as SQL before publication.
            state.set_root(&session)?;
            state.intent = None;
            state.receipts.insert(
                identity.key.clone(),
                SessionIdempotencyReceipt::new(
                    identity.payload_hash.clone(),
                    SESSION_CREATE_REVISION,
                    SessionCommandReceiptKind::Root,
                ),
            );
            state.append(facts.clone());
            if self.persist(observed.as_ref(), owner, &id, &state).await? {
                return Ok(SessionCreateResult::Applied(session));
            }
            session.revision = SessionRevision(0);
        }
        Err(busy())
    }
    async fn replay_create(
        &self,
        owner: &str,
        id: &SessionId,
        identity: &IdempotencyRecord,
    ) -> Result<Option<PersistedSession>, SessionRepositoryError> {
        if owner.trim().is_empty()
            || id.as_str().trim().is_empty()
            || identity.key.trim().is_empty()
            || identity.payload_hash.trim().is_empty()
        {
            return Err(SessionRepositoryError::InvalidMutation(
                "invalid Session create replay query".into(),
            ));
        }
        let Some(object) = self.store.load(id.as_str()).await? else {
            return Ok(None);
        };
        let state = State::decode(&object)?;
        classify_create_replay(
            owner,
            id.as_str(),
            identity,
            state.receipts.get(&identity.key).cloned(),
            state.identity(&object.owner_scope)?,
            MissingCreateReceipt::RejectOccupied,
        )
    }
    async fn commit(
        &self,
        owner: &str,
        mutation: SessionMutation,
    ) -> Result<SessionMutationResult, SessionRepositoryError> {
        self.commit_root(owner, mutation, None).await
    }
    async fn idempotency_receipt(
        &self,
        id: &SessionId,
        key: &str,
    ) -> Result<Option<SessionIdempotencyReceipt>, SessionRepositoryError> {
        let Some(object) = self.store.load(id.as_str()).await? else {
            return Ok(None);
        };
        Ok(State::decode(&object)?.receipts.get(key).cloned())
    }
    async fn count_environment_phase(
        &self,
        phase: SessionEnvironmentPhase,
    ) -> Result<u64, SessionRepositoryError> {
        let mut count = 0_u64;
        for object in self.objects().await? {
            let state = State::decode(&object)?;
            if state.root.is_some()
                && state
                    .session(&object.session_id)?
                    .environment
                    .step()
                    .phase()
                    == phase
            {
                count = count
                    .checked_add(1)
                    .ok_or_else(|| corrupt("Session count overflow"))?;
            }
        }
        Ok(count)
    }
}
#[async_trait]
impl SessionReconciliation for ObjectManagedSessionRepository {
    async fn reconciliation_scan(
        &self,
        after: Option<&SessionScanCursor>,
    ) -> Result<SessionScan, SessionRepositoryError> {
        let mut page = SessionScan::default();
        let mut examined = 0;
        for mut object in self.objects().await? {
            if after.is_some_and(|a| object.session_id.as_str() <= a.session_id().as_str()) {
                continue;
            }
            let state = State::decode(&object)?;
            // The persisted hint was computed by the writer's domain version.
            // Inspect every live root so upgrades cannot hide newly required work.
            if state.root.is_none() {
                continue;
            }
            if examined == RECOVERY_BATCH_SIZE as usize {
                return Ok(page);
            }
            examined += 1;
            page.next_cursor = Some(SessionScanCursor::after_session_id(&object.session_id));
            for retry in 0..32 {
                if retry == 31 {
                    return Err(busy());
                }
                let mut state = State::decode(&object)?;
                if state.root.is_none() {
                    break;
                }
                match state.session(&object.session_id) {
                    Ok(session) => {
                        let reconcilable = session.needs_reconciliation();
                        let repair = state.quarantined.take().is_some()
                            || state.reconcilable != reconcilable;
                        state.reconcilable = reconcilable;
                        if repair
                            && !self
                                .persist(
                                    Some(&object),
                                    &object.owner_scope,
                                    &object.session_id,
                                    &state,
                                )
                                .await?
                        {
                            object = self
                                .store
                                .load(&object.session_id)
                                .await?
                                .ok_or(SessionRepositoryError::NotFound)?;
                            continue;
                        }
                        if reconcilable {
                            page.sessions.push(ScopedPersistedSession {
                                workspace_id: object.owner_scope.clone(),
                                session,
                            });
                        }
                    }
                    Err(error) => {
                        let reason = error.to_string();
                        state.quarantined = Some(reason.clone());
                        if !self
                            .persist(
                                Some(&object),
                                &object.owner_scope,
                                &object.session_id,
                                &state,
                            )
                            .await?
                        {
                            object = self
                                .store
                                .load(&object.session_id)
                                .await?
                                .ok_or(SessionRepositoryError::NotFound)?;
                            continue;
                        }
                        page.quarantined.push(SessionRecoveryQuarantine {
                            session_id: object.session_id.clone().into(),
                            reason,
                        });
                    }
                }
                break;
            }
        }
        page.next_cursor = None;
        Ok(page)
    }
}
#[async_trait]
impl LifecycleOutbox for ObjectManagedSessionRepository {
    async fn append_lifecycle(
        &self,
        fact: ManagedLifecycleFact,
    ) -> Result<(), SessionRepositoryError> {
        let owner = fact.workspace_id.as_deref().ok_or_else(|| {
            SessionRepositoryError::InvalidMutation(
                "object outbox requires the persisted Workspace owner".into(),
            )
        })?;
        for _ in 0..32 {
            let observed = self.store.load(&fact.object_id).await?;
            let mut state = observed
                .as_ref()
                .map(State::decode)
                .transpose()?
                .unwrap_or_default();
            if observed.as_ref().is_some_and(|o| o.owner_scope != owner) {
                return Err(SessionRepositoryError::InvalidMutation(
                    "outbox owner mismatch".into(),
                ));
            }
            if state.outbox.iter().any(|(f, _)| f.id == fact.id) {
                return Ok(());
            }
            state.append([fact.clone()]);
            if self
                .persist(observed.as_ref(), owner, &fact.object_id, &state)
                .await?
            {
                return Ok(());
            }
        }
        Err(busy())
    }
    async fn pending_lifecycle(&self) -> Result<Vec<ManagedLifecycleFact>, SessionRepositoryError> {
        let mut result = Vec::new();
        for object in self.objects().await? {
            result.extend(
                State::decode(&object)?
                    .outbox
                    .into_iter()
                    .filter(|(_, done)| !done)
                    .map(|(fact, _)| fact),
            );
        }
        result.sort_by(|a, b| (a.timestamp, &a.id).cmp(&(b.timestamp, &b.id)));
        Ok(result)
    }
    async fn complete_lifecycle(&self, id: &str) -> Result<(), SessionRepositoryError> {
        for observed in self.objects().await? {
            let mut object = observed;
            for retry in 0..32 {
                if retry == 31 {
                    return Err(busy());
                }
                let mut state = State::decode(&object)?;
                let Some((_, done)) = state.outbox.iter_mut().find(|(f, _)| f.id == id) else {
                    break;
                };
                if *done {
                    return Ok(());
                }
                *done = true;
                if self
                    .persist(
                        Some(&object),
                        &object.owner_scope,
                        &object.session_id,
                        &state,
                    )
                    .await?
                {
                    return Ok(());
                }
                object = self
                    .store
                    .load(&object.session_id)
                    .await?
                    .ok_or(SessionRepositoryError::NotFound)?;
            }
        }
        Ok(())
    }
}
#[async_trait]
impl PreparedParticipantIntents for ObjectManagedSessionRepository {
    async fn record_creation_intent(
        &self,
        owner: &str,
        id: &SessionId,
        repository: &str,
        expires: u64,
    ) -> Result<PreparedParticipantIntentRecord, SessionRepositoryError> {
        creation_intents::db_ms(expires)?;
        for _ in 0..32 {
            let observed = self.store.load(id.as_str()).await?;
            let mut state = observed
                .as_ref()
                .map(State::decode)
                .transpose()?
                .unwrap_or_default();
            if observed.as_ref().is_some_and(|o| o.owner_scope != owner) {
                return Err(SessionRepositoryError::Conflict(
                    SessionRepositoryConflict::AlreadyExists,
                ));
            }
            if state.root.is_some() {
                return Ok(PreparedParticipantIntentRecord::RootExists);
            }
            if state.tombstone.is_some()
                || state
                    .intent
                    .as_ref()
                    .is_some_and(|i| i.phase != creation_intents::PREPARING)
            {
                return Err(SessionRepositoryError::Conflict(
                    SessionRepositoryConflict::Tombstoned,
                ));
            }
            let intent = state.intent.get_or_insert_with(|| Intent {
                repositories: Vec::new(),
                phase: creation_intents::PREPARING.into(),
                expires,
            });
            if !intent.repositories.iter().any(|r| r == repository) {
                intent.repositories.push(repository.into());
            }
            intent.expires = intent.expires.max(expires);
            if self
                .persist(observed.as_ref(), owner, id.as_str(), &state)
                .await?
            {
                return Ok(PreparedParticipantIntentRecord::Recorded);
            }
        }
        Err(busy())
    }
    async fn claim_creation_abort(
        &self,
        id: &SessionId,
    ) -> Result<PreparedParticipantIntentClaim, SessionRepositoryError> {
        for _ in 0..32 {
            let Some(object) = self.store.load(id.as_str()).await? else {
                return Ok(PreparedParticipantIntentClaim::Absent);
            };
            let mut state = State::decode(&object)?;
            let Some(intent) = state.intent.as_mut() else {
                return Ok(PreparedParticipantIntentClaim::Absent);
            };
            if intent.phase == creation_intents::PREPARING {
                intent.phase = creation_intents::ABORTING.into();
            }
            let result = state
                .intent(&object)?
                .ok_or_else(|| corrupt("creation intent disappeared"))?;
            if self
                .persist(Some(&object), &object.owner_scope, id.as_str(), &state)
                .await?
            {
                return Ok(PreparedParticipantIntentClaim::Claimed(result));
            }
        }
        Err(busy())
    }
    async fn complete_creation_abort(&self, id: &SessionId) -> Result<(), SessionRepositoryError> {
        for _ in 0..32 {
            let Some(object) = self.store.load(id.as_str()).await? else {
                return Ok(());
            };
            let mut state = State::decode(&object)?;
            let Some(intent) = state.intent.as_mut() else {
                return Ok(());
            };
            if intent.phase == creation_intents::PREPARING {
                return Err(SessionRepositoryError::InvalidMutation(
                    "abort has not been claimed".into(),
                ));
            }
            intent.phase = creation_intents::ABORTED.into();
            if self
                .persist(Some(&object), &object.owner_scope, id.as_str(), &state)
                .await?
            {
                return Ok(());
            }
        }
        Err(busy())
    }
    async fn recoverable_creation_intents(
        &self,
        now: u64,
        limit: usize,
    ) -> Result<Vec<PreparedParticipantIntent>, SessionRepositoryError> {
        let mut result = Vec::new();
        for object in self.objects().await? {
            if let Some(intent) = State::decode(&object)?.intent(&object)?
                && (intent.phase() == PreparedParticipantIntentPhase::Aborting
                    || (intent.phase() == PreparedParticipantIntentPhase::Preparing
                        && intent.lease_expires_at_unix_ms() <= now))
            {
                result.push(intent);
            }
        }
        result.sort_by(|a, b| {
            (a.lease_expires_at_unix_ms(), a.session_id())
                .cmp(&(b.lease_expires_at_unix_ms(), b.session_id()))
        });
        result.truncate(limit);
        Ok(result)
    }
}

#[derive(Clone)]
struct ObjectPublication(Arc<dyn SessionObjectStore>);
fn publication_error(error: awaken_object_storage::PublicationError) -> SessionRepositoryError {
    use awaken_object_storage::PublicationError;
    match error {
        PublicationError::AlreadyExists => {
            SessionRepositoryError::Conflict(SessionRepositoryConflict::AlreadyExists)
        }
        PublicationError::NotApplied(detail) => SessionRepositoryError::NotApplied(detail),
        PublicationError::AuthorityUnavailable(detail) => {
            SessionRepositoryError::AuthorityUnavailable(detail)
        }
        PublicationError::Corrupt(detail) => SessionRepositoryError::Corrupt(detail),
        PublicationError::InvalidMutation(detail) => {
            SessionRepositoryError::InvalidMutation(detail)
        }
    }
}
impl ObjectPublication {
    async fn load(&self, id: &str) -> Result<Option<SessionObject>, SessionRepositoryError> {
        self.0.load(id).await.map_err(publication_error)
    }
    async fn list(&self) -> Result<Vec<SessionObject>, SessionRepositoryError> {
        self.0.list().await.map_err(publication_error)
    }
    async fn insert(
        &self,
        owner: &str,
        id: &str,
        update: &SessionObjectUpdate,
    ) -> Result<bool, SessionRepositoryError> {
        self.0
            .insert(owner, id, update)
            .await
            .map_err(publication_error)
    }
    async fn replace(
        &self,
        object: &SessionObject,
        update: &SessionObjectUpdate,
    ) -> Result<bool, SessionRepositoryError> {
        self.0
            .replace(object, update)
            .await
            .map_err(publication_error)
    }
}
