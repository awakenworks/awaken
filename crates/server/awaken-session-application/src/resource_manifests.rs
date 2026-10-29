// @formal entrypoint session_resources.failed_replacement_preserves_active_manifest
//! The Session Resource desired-manifest command: its idempotent replay, its
//! root-CAS replacement, and the convergence that follows an accepted
//! generation.
//!
//! Recovery and terminal reclamation own the durable sweep; this module owns
//! the caller-issued Resource commands (complete-manifest replacement, live
//! File attachment, input detachment) and the exact receipt they return. A
//! protocol lowers its wire resources into `SessionInputRequest`s and projects
//! the outcome; `resource_lowering` makes every identity and reuse decision.

use crate::mutation::ROOT_CAS_ATTEMPTS;
use crate::repository_participants::RepositoryRetirementCause;
use awaken_agent_contract::stable_fingerprint;
use awaken_session_state_contract::{IdempotencyRecord, SessionExecutionState};
use awaken_session_types::RunError;
use awaken_time::WallClockExt as _;

use crate::mutation::repository_failure;
use crate::resource_lowering::{ResourceLowering, live_file_binding_id, normalized_mount};
use crate::session_input::{MAX_SESSION_FILE_RESOURCES, input_binding};
use crate::session_resource::{
    ResolvedInputSource, ResolvedSessionResources, SessionInputAttachment,
};
use crate::{
    SessionInputRequest, SessionInputTarget, SessionMutationError, SessionPreparationError,
};
use awaken_resource_contract::BindingId;
use awaken_session_state_contract::PersistedSession;
use awaken_session_types::{SessionId, SessionRevision};
use std::sync::Arc;

fn resource_manifest_operation_id(session_id: &str, idempotency_key: &str) -> String {
    stable_fingerprint(&(session_id, idempotency_key))
}

fn resource_manifest_idempotency_record(
    session_id: &str,
    idempotency_key: &str,
    request_fingerprint: &str,
) -> IdempotencyRecord {
    IdempotencyRecord {
        key: format!(
            "managed:resource-manifest-command:{session_id}:{}",
            resource_manifest_operation_id(session_id, idempotency_key)
        ),
        payload_hash: request_fingerprint.to_string(),
    }
}

/// Protocol-neutral whole-manifest command. The interface owns request
/// canonicalization; the Session application owns desired-state persistence,
/// idempotency, and realization ordering.
#[derive(Clone, Debug)]
pub struct ReplaceSessionResourceManifest {
    pub resources: ResolvedSessionResources,
    /// Optimistic fence for the Resource sub-aggregate, not the Session root.
    /// Runtime realization lease renewals advance the root revision without
    /// changing this generation and therefore must not starve Resource writes.
    pub expected_resource_revision: Option<u64>,
    pub idempotency_key: Option<String>,
    pub request_fingerprint: String,
}

/// Durable command receipt plus the latest aggregate projection. The command
/// revision is stable across replay even if realization advances the root CAS.
#[derive(Clone, Debug)]
pub struct SessionResourceManifestOutcome {
    pub session: PersistedSession,
    pub command_revision: SessionRevision,
    pub command_applied: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionResourceManifestError {
    #[error("Session was not found")]
    NotFound,
    #[error("Session is terminal")]
    Terminal,
    #[error("Session revision conflict")]
    Conflict,
    #[error("Session idempotency key was reused with another manifest")]
    IdempotencyMismatch,
    #[error("Session resource manifest was rejected: {0}")]
    Rejected(#[source] RunError),
    /// Desired truth already committed; a retry/reconciler may repair the
    /// disposable Runtime projection without accepting another generation.
    #[error("Session resource realization failed after commit: {source}")]
    ProjectionAfterCommit {
        outcome: Box<SessionResourceManifestOutcome>,
        #[source]
        source: SessionPreparationError,
    },
    #[error("Session resource persistence is unavailable: {0}")]
    Unavailable(String),
}

impl SessionResourceManifestError {
    pub(crate) fn mutation(error: SessionMutationError) -> Self {
        match error {
            SessionMutationError::NotFound => Self::NotFound,
            SessionMutationError::Conflict => Self::Conflict,
            SessionMutationError::IdempotencyMismatch => Self::IdempotencyMismatch,
            SessionMutationError::Unavailable(message) => Self::Unavailable(message),
            error @ SessionMutationError::Internal(_) => Self::Rejected(error.into()),
        }
    }
}

/// The Session Resource commands: replace the complete desired manifest,
/// attach a live File and detach an input, each with its exact idempotent
/// receipt, then converge the accepted generation. It holds the CAS kernel,
/// the clock, the Resource commit bracket, the Session Work projection, the
/// Repository participants and Resource reconciliation, plus the Resource
/// Registry and realization-effects role the request lowering reads.
pub struct SessionResourceManifests {
    mutations: Arc<crate::SessionMutations>,
    clock: Arc<dyn awaken_time::WallClock>,
    resource_references: Arc<crate::SessionResourceReferences>,
    work_assignments: Arc<crate::SessionWorkAssignments>,
    repository_participants: Arc<crate::SessionRepositoryParticipants>,
    resource_reconciliation: Arc<crate::SessionResourceReconciliation>,
    resource_registry: Option<Arc<dyn awaken_resource_contract::ResourceRegistry>>,
    placement: awaken_session_execution_contract::SessionPlacementRoles,
}

/// The collaborators one [`SessionResourceManifests`] is built from.
pub(crate) struct SessionResourceManifestCollaborators {
    pub(crate) mutations: Arc<crate::SessionMutations>,
    pub(crate) clock: Arc<dyn awaken_time::WallClock>,
    pub(crate) resource_references: Arc<crate::SessionResourceReferences>,
    pub(crate) work_assignments: Arc<crate::SessionWorkAssignments>,
    pub(crate) repository_participants: Arc<crate::SessionRepositoryParticipants>,
    pub(crate) resource_reconciliation: Arc<crate::SessionResourceReconciliation>,
    pub(crate) resource_registry: Option<Arc<dyn awaken_resource_contract::ResourceRegistry>>,
    pub(crate) placement: awaken_session_execution_contract::SessionPlacementRoles,
}

impl SessionResourceManifests {
    pub(crate) fn new(collaborators: SessionResourceManifestCollaborators) -> Self {
        let SessionResourceManifestCollaborators {
            mutations,
            clock,
            resource_references,
            work_assignments,
            repository_participants,
            resource_reconciliation,
            resource_registry,
            placement,
        } = collaborators;
        Self {
            mutations,
            clock,
            resource_references,
            work_assignments,
            repository_participants,
            resource_reconciliation,
            resource_registry,
            placement,
        }
    }

    /// The Resource commit bracket this component commits through.
    #[cfg(test)]
    pub(crate) fn resource_references(&self) -> &crate::SessionResourceReferences {
        &self.resource_references
    }

    /// The request lowering over this component's Resource authorities.
    pub(crate) fn lowering<'a>(
        &'a self,
        session_id: &'a str,
        owner_scope: &'a str,
    ) -> ResourceLowering<'a> {
        ResourceLowering::new(
            self.resource_registry.as_deref(),
            &self.repository_participants,
            &self.placement,
            session_id,
            owner_scope,
        )
    }

    /// Resolve an exact command replay before an interface performs lowering
    /// side effects such as Repository/Vault configuration. The main command
    /// repeats this check to close the race with a concurrent first writer.
    pub async fn replay_session_resource_manifest(
        &self,
        session_id: &awaken_session_types::SessionId,
        idempotency_key: &str,
        request_fingerprint: &str,
    ) -> Result<Option<SessionResourceManifestOutcome>, SessionResourceManifestError> {
        let record = resource_manifest_idempotency_record(
            session_id.as_str(),
            idempotency_key,
            request_fingerprint,
        );
        let Some(receipt) = self
            .mutations
            .repository()
            .idempotency_receipt(session_id, &record.key)
            .await
            .map_err(repository_failure)
            .map_err(SessionResourceManifestError::mutation)?
        else {
            return Ok(None);
        };
        if receipt.payload_hash != record.payload_hash {
            return Err(SessionResourceManifestError::IdempotencyMismatch);
        }
        let session = self
            .mutations
            .session_aggregate(session_id)
            .await
            .map_err(repository_failure)
            .map_err(SessionResourceManifestError::mutation)?;
        self.converge_resource_manifest(SessionResourceManifestOutcome {
            session: session.into_snapshot(),
            command_revision: receipt.committed_revision,
            command_applied: false,
        })
        .await
        .map(Some)
    }

    /// Persist one complete desired Resource manifest through the Session root
    /// CAS, then ask the existing local reconciler or WorkQueue projection to
    /// realize that exact generation. No external effect runs before this
    /// method has a durable, queryable desired manifest.
    pub async fn replace_session_resource_manifest(
        &self,
        session_id: &awaken_session_types::SessionId,
        command: ReplaceSessionResourceManifest,
    ) -> Result<SessionResourceManifestOutcome, SessionResourceManifestError> {
        let command_record = command.idempotency_key.as_ref().map(|key| {
            resource_manifest_idempotency_record(
                session_id.as_str(),
                key,
                &command.request_fingerprint,
            )
        });

        if let Some(key) = &command.idempotency_key
            && let Some(outcome) = self
                .replay_session_resource_manifest(session_id, key, &command.request_fingerprint)
                .await?
        {
            return Ok(outcome);
        }

        for attempt in 0..ROOT_CAS_ATTEMPTS {
            match self
                .replace_session_resource_manifest_once(
                    session_id,
                    &command,
                    command_record.clone(),
                )
                .await
            {
                Err(SessionResourceManifestError::Conflict) if attempt + 1 < ROOT_CAS_ATTEMPTS => {
                    continue;
                }
                Ok(outcome) => return self.converge_resource_manifest(outcome).await,
                result => return result,
            }
        }
        Err(SessionResourceManifestError::Conflict)
    }

    async fn replace_session_resource_manifest_once(
        &self,
        session_id: &awaken_session_types::SessionId,
        command: &ReplaceSessionResourceManifest,
        command_record: Option<IdempotencyRecord>,
    ) -> Result<SessionResourceManifestOutcome, SessionResourceManifestError> {
        let mutations = &self.mutations;
        let owner_scope = mutations
            .owner(session_id)
            .await
            .map_err(SessionResourceManifestError::mutation)?;
        let mut session = mutations
            .session_aggregate(session_id)
            .await
            .map_err(repository_failure)
            .map_err(SessionResourceManifestError::mutation)?;
        if session.is_terminal() {
            return Err(SessionResourceManifestError::Terminal);
        }
        if command
            .expected_resource_revision
            .is_some_and(|expected| expected != session.resources.revision)
        {
            return Err(SessionResourceManifestError::Conflict);
        }
        let mut desired = command.resources.clone();
        desired.observe_input_times(session.resources.desired(), self.clock.now_ms());
        if let Some(baseline) = session.frozen_baseline()
            && !baseline
                .mutation_policy
                .admits_resource_replacement(session.resources.desired(), &desired)
        {
            return Err(SessionResourceManifestError::Rejected(
                RunError::bad_request(
                    "profiled Session resource mutation is outside its immutable policy",
                ),
            ));
        }

        let unchanged = session.resources.desired() == &desired;
        if !unchanged {
            let result = if session.resources.pending.is_some() {
                session
                    .resources()
                    .revise_unattempted_pending(desired.clone())
            } else {
                session.resources().prepare(desired)
            };
            result.map_err(|error| match error {
                awaken_session_state_contract::ResourceActivationError::Pending => {
                    SessionResourceManifestError::Conflict
                }
                error => {
                    SessionResourceManifestError::Rejected(RunError::bad_request(error.to_string()))
                }
            })?;
        }

        let (session, command_applied, command_revision) = match command_record {
            Some(record) => {
                let commit = self
                    .resource_references
                    .commit_with_record(&owner_scope, session, record, Vec::new())
                    .await
                    .map_err(SessionResourceManifestError::mutation)?;
                let applied = commit.transitioned();
                let committed_revision = commit.committed_revision();
                (commit.into_session(), applied, committed_revision)
            }
            None if unchanged => {
                let revision = session.revision;
                return Ok(SessionResourceManifestOutcome {
                    session: session.into_snapshot(),
                    command_revision: revision,
                    command_applied: false,
                });
            }
            None => {
                let session = self
                    .resource_references
                    .commit(
                        &owner_scope,
                        session,
                        "resource-manifest-intent",
                        Vec::new(),
                    )
                    .await
                    .map_err(SessionResourceManifestError::mutation)?
                    .into_snapshot();
                let committed_revision = session.revision;
                (session, true, committed_revision)
            }
        };
        Ok(SessionResourceManifestOutcome {
            session,
            command_revision,
            command_applied,
        })
    }

    async fn converge_resource_manifest(
        &self,
        mut outcome: SessionResourceManifestOutcome,
    ) -> Result<SessionResourceManifestOutcome, SessionResourceManifestError> {
        if outcome.session.resources.pending.is_none()
            && !outcome.session.resources.has_repository_retirements()
        {
            return Ok(outcome);
        }
        let owner_scope = self
            .mutations
            .owner(&outcome.session.session_id)
            .await
            .map_err(SessionResourceManifestError::mutation)?;
        let convergence = if outcome.session.resources.pending.is_none() {
            self.repository_participants
                .reconcile_retirements(
                    &owner_scope,
                    crate::mutation::aggregate_from_snapshot(outcome.session.clone())
                        .map_err(repository_failure)
                        .map_err(SessionResourceManifestError::mutation)?,
                    RepositoryRetirementCause::DetachedGeneration,
                )
                .await
                .map(crate::SessionAggregate::into_snapshot)
        } else if outcome.session.requires_external_environment_owner() {
            self.work_assignments
                .dispatch(&outcome.session)
                .await
                .map(|_| outcome.session.clone())
                .map_err(|error| SessionPreparationError::Unavailable(error.to_string()))
        } else if outcome.session.execution == SessionExecutionState::Idle {
            self.resource_reconciliation
                .reconcile_persisted_resources(&owner_scope, outcome.session.clone())
                .await
        } else {
            Ok(outcome.session.clone())
        };
        match convergence {
            Ok(session) => {
                outcome.session = session;
                Ok(outcome)
            }
            Err(source) => Err(SessionResourceManifestError::ProjectionAfterCommit {
                outcome: Box::new(outcome),
                source,
            }),
        }
    }
}

/// Item-level Resource verbs never add or remove a MemoryStore: it is bound
/// when the Session is created.
pub const MEMORY_STORE_CREATE_ONLY: &str = "memory stores can only be attached at session creation time; adding or removing one from a \
     running session is not supported";

/// One complete authored Resource manifest, lowered from a protocol's wire
/// resources. The fingerprint identifies the authored request for idempotent
/// replay; the protocol computes it over its own wire shape.
#[derive(Clone, Debug)]
pub struct SessionResourceManifestRequest {
    pub inputs: Vec<SessionInputRequest>,
    /// Optimistic fence for the Resource sub-aggregate.
    pub expected_resource_revision: Option<u64>,
    pub idempotency_key: Option<String>,
    pub request_fingerprint: String,
}

/// The accepted live File attachment and the binding it occupies.
#[derive(Clone, Debug)]
pub struct SessionResourceAttachment {
    pub outcome: SessionResourceManifestOutcome,
    pub binding_id: BindingId,
}

fn rejected(error: impl std::fmt::Display) -> SessionResourceManifestError {
    SessionResourceManifestError::Rejected(RunError::bad_request(error.to_string()))
}

/// The owner and durable root one Resource command reads before deciding.
async fn read_resource_root(
    mutations: &crate::SessionMutations,
    session_id: &SessionId,
) -> Result<(String, PersistedSession), SessionResourceManifestError> {
    let owner_scope = mutations
        .owner(session_id)
        .await
        .map_err(SessionResourceManifestError::mutation)?;
    let session = mutations
        .session_aggregate(session_id)
        .await
        .map_err(crate::mutation::repository_failure)
        .map_err(SessionResourceManifestError::mutation)?
        .into_snapshot();
    Ok((owner_scope, session))
}

/// Item verbs replace the whole desired manifest fenced by the Resource
/// revision they read; they configure no Repository participant.
fn item_verb_manifest(
    desired: ResolvedSessionResources,
    read_revision: u64,
) -> ReplaceSessionResourceManifest {
    ReplaceSessionResourceManifest {
        request_fingerprint: stable_fingerprint(&desired),
        resources: desired,
        expected_resource_revision: Some(read_revision),
        idempotency_key: None,
    }
}

impl SessionResourceManifests {
    /// Replace the complete authored Resource manifest.
    ///
    /// | Rule | Cause | Effect |
    /// |---|---|---|
    /// | M1 | exact idempotent replay | the recorded outcome; no lowering |
    /// | M2 | expected Resource revision differs | `Conflict`; no lowering |
    /// | M3 | profiled (non-managed) mutation policy | rejected; no lowering |
    /// | M4 | lowering or the root command fails | the configured Repository prefix is compensated; the first error |
    /// | M5 | accepted | the committed outcome |
    pub async fn replace_session_resource_manifest_request(
        &self,
        session_id: &SessionId,
        request: SessionResourceManifestRequest,
    ) -> Result<SessionResourceManifestOutcome, SessionResourceManifestError> {
        if let Some(key) = request.idempotency_key.as_deref()
            && let Some(outcome) = self
                .replay_session_resource_manifest(session_id, key, &request.request_fingerprint)
                .await?
        {
            return Ok(outcome);
        }
        let (owner_scope, persisted) = read_resource_root(&self.mutations, session_id).await?;
        if request
            .expected_resource_revision
            .is_some_and(|expected| expected != persisted.resources.revision)
        {
            return Err(SessionResourceManifestError::Conflict);
        }
        if persisted
            .frozen_baseline()
            .is_some_and(|baseline| !baseline.mutation_policy.is_managed())
        {
            // Complete-manifest lowering may configure Repository/Vault state.
            // Profiled Sessions use item-level File verbs when their immutable
            // policy admits them, so reject before any lowering side effect.
            return Err(SessionResourceManifestError::Rejected(
                RunError::bad_request(
                    "profiled Session whole-resource manifest replacement is unavailable",
                ),
            ));
        }
        let mut lowering = self.lowering(session_id.as_str(), &owner_scope);
        let resources = lowering
            .manifest(&request.inputs, &persisted)
            .await
            .map_err(SessionResourceManifestError::Rejected)?;
        let result = Box::pin(self.replace_session_resource_manifest(
            session_id,
            ReplaceSessionResourceManifest {
                resources,
                expected_resource_revision: request.expected_resource_revision,
                idempotency_key: request.idempotency_key,
                request_fingerprint: request.request_fingerprint,
            },
        ))
        .await;
        // Desired truth that committed keeps its participants even when the
        // later realization failed; any other refusal compensates them.
        if let Err(error) = &result
            && !matches!(
                error,
                SessionResourceManifestError::ProjectionAfterCommit { .. }
            )
        {
            lowering
                .compensate(
                    "Resource Manifest Repository compensation remains pending after root rejection",
                )
                .await;
        }
        result
    }

    /// Attach one live File input.
    ///
    /// | Rule | Cause | Effect |
    /// |---|---|---|
    /// | A1 | the target is not a File | rejected |
    /// | A2 | the same File already occupies the mount | the current manifest re-driven; its existing binding (an internal error if convergence dropped it) |
    /// | A3 | the File limit is reached | rejected |
    /// | A4 | otherwise | a fresh live binding attached through the manifest command |
    pub async fn attach_session_file(
        &self,
        session_id: &SessionId,
        request: SessionInputRequest,
    ) -> Result<SessionResourceAttachment, SessionResourceManifestError> {
        let SessionInputTarget::File(requested_file) = &request.target else {
            return Err(rejected(
                "a live Session Resource attachment must be a File",
            ));
        };
        let (owner_scope, persisted) = read_resource_root(&self.mutations, session_id).await?;
        let read_revision = persisted.resources.revision;
        let current = persisted.resources.desired().clone();
        let mount = normalized_mount(&request.mount_path);
        if let Some(existing) = current.inputs().iter().find(|input| {
            normalized_mount(&input.mount_path) == mount
                && matches!(
                    &input.source,
                    ResolvedInputSource::File { file_id } if file_id == requested_file
                )
        }) {
            // An earlier response may have been lost after desired truth
            // committed but before realization settled. Re-drive that exact
            // generation through the canonical Manifest command; never allocate
            // a second binding or report a misleading mount collision.
            let binding_id = existing.binding_id.clone();
            let outcome = Box::pin(self.replace_session_resource_manifest(
                session_id,
                item_verb_manifest(current, read_revision),
            ))
            .await?;
            if !outcome
                .session
                .resources
                .desired()
                .inputs()
                .iter()
                .any(|input| input.binding_id == binding_id)
            {
                return Err(SessionResourceManifestError::Rejected(RunError::internal(
                    "replayed Resource binding disappeared after Manifest convergence",
                )));
            }
            return Ok(SessionResourceAttachment {
                outcome,
                binding_id,
            });
        }
        if current
            .inputs()
            .iter()
            .filter(|input| matches!(input.source, ResolvedInputSource::File { .. }))
            .count()
            >= MAX_SESSION_FILE_RESOURCES
        {
            return Err(rejected(format!(
                "a Session supports at most {MAX_SESSION_FILE_RESOURCES} files"
            )));
        }
        let lowering = self.lowering(session_id.as_str(), &owner_scope);
        let input = lowering
            .resolve(SessionInputAttachment {
                binding: input_binding(
                    live_file_binding_id(session_id.as_str(), &current),
                    &request,
                    None,
                ),
                replaces: None,
            })
            .await
            .map_err(SessionResourceManifestError::Rejected)?;
        let binding_id = input.binding_id.clone();
        let desired = current.attach(input).map_err(rejected)?;
        lowering
            .validate_layout(&persisted, desired.sandbox_layout_bindings())
            .map_err(SessionResourceManifestError::Rejected)?;
        let outcome = Box::pin(self.replace_session_resource_manifest(
            session_id,
            item_verb_manifest(desired, read_revision),
        ))
        .await?;
        Ok(SessionResourceAttachment {
            outcome,
            binding_id,
        })
    }

    /// Detach one input.
    ///
    /// | Rule | Cause | Effect |
    /// |---|---|---|
    /// | D1 | no input occupies the binding | `NotFound` |
    /// | D2 | the input is a MemoryStore | rejected with [`MEMORY_STORE_CREATE_ONLY`] |
    /// | D3 | otherwise | the input detached through the manifest command |
    pub async fn detach_session_input(
        &self,
        session_id: &SessionId,
        binding_id: &BindingId,
    ) -> Result<SessionResourceManifestOutcome, SessionResourceManifestError> {
        let (_, persisted) = read_resource_root(&self.mutations, session_id).await?;
        let current = persisted.resources.desired();
        let input = current
            .inputs()
            .iter()
            .find(|input| &input.binding_id == binding_id)
            .ok_or(SessionResourceManifestError::NotFound)?;
        if matches!(input.source, ResolvedInputSource::MemoryStore { .. }) {
            return Err(rejected(MEMORY_STORE_CREATE_ONLY));
        }
        let (desired, _) = current.detach(binding_id).map_err(rejected)?;
        Box::pin(self.replace_session_resource_manifest(
            session_id,
            item_verb_manifest(desired, persisted.resources.revision),
        ))
        .await
    }
}
