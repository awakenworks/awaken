//! Claim-fenced Memory snapshot and CAS write-back adapters.
//!
//! This is a Memory-specific Worker boundary. It carries one frozen
//! Workspace/store/config/access binding and delegates to the canonical
//! [`awaken_memory_store::MemoryRepository`]. It exposes no store authoring,
//! history redaction, retention, or lifecycle operation.

use std::sync::Arc;

use awaken_resource_contract::{
    ConfigVersion, LiveResourceBindingVerifier, MemErr, Memory,
    MemoryMaterializationReferenceEncoder, MemoryMaterializationReferenceError, MemoryRepository,
    ResourceAccess,
};
use awaken_dispatch_contract::{DispatchQueue, ResourceOperationFence, RunClaim};
use awaken_session_contract::{
    SessionRealizationControl, SessionResourceMaterializationIntent, SessionTerminalMemoryIntent,
    SessionTerminalMemoryTarget,
};
use awaken_worker_contract::{WorkerDirectory, WorkerIdentity};
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use awaken_worker_transport_security::{
    VerifiedWorkerContext, WorkerRequestAuthenticator, WorkerUpstream, authenticate_worker_request,
};

use crate::worker_authority::{
    SessionWorkerEffectTemporalRule, unix_now_ms, verify_session_worker_effect,
};

const MEMORY_PATH: &str = "/v1/worker/resources/memory/operation";

const MEMORY_REFERENCE_V1_PREFIX: &str = "awaken-memory-v1:";
const MEMORY_REFERENCE_V2_PREFIX: &str = "awaken-memory-v2:";
const MEMORY_REFERENCE_V3_PREFIX: &str = "awaken-memory-v3:";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct RunMemoryMaterializationReferenceV1 {
    workspace_id: String,
    memory_store_id: String,
    config_version: ConfigVersion,
    access: ResourceAccess,
    claim: RunClaim,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct TerminalMemoryMaterializationReferenceV2 {
    binding_id: awaken_resource_contract::BindingId,
    config_version: ConfigVersion,
    access: ResourceAccess,
    materialization: awaken_provisioning_contract::MemoryMaterializationEvidence,
    fence: ResourceOperationFence,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum MemoryMaterializationReference {
    RunV1(RunMemoryMaterializationReferenceV1),
    TerminalV2(Box<SessionTerminalMemoryIntent>),
    RealizationV3(Box<SessionResourceMaterializationIntent>),
}

/// Stateless encoder for the Resource Worker HTTP wire capability.
#[derive(Debug, Default)]
pub struct HttpMemoryMaterializationReferenceEncoder;

impl MemoryMaterializationReferenceEncoder<RunClaim> for HttpMemoryMaterializationReferenceEncoder {
    fn encode(
        &self,
        workspace_id: &str,
        memory_store_id: &str,
        config_version: ConfigVersion,
        access: ResourceAccess,
        claim: &RunClaim,
    ) -> Result<String, MemoryMaterializationReferenceError> {
        memory_materialization_reference(
            workspace_id,
            memory_store_id,
            config_version,
            access,
            claim,
        )
    }
}

impl MemoryMaterializationReferenceEncoder<SessionTerminalMemoryIntent>
    for HttpMemoryMaterializationReferenceEncoder
{
    fn encode(
        &self,
        _workspace_id: &str,
        memory_store_id: &str,
        config_version: ConfigVersion,
        access: ResourceAccess,
        intent: &SessionTerminalMemoryIntent,
    ) -> Result<String, MemoryMaterializationReferenceError> {
        if memory_store_id != intent.memory_store_id()
            || config_version != intent.config_version()
            || access != intent.access()
        {
            return Err(MemoryMaterializationReferenceError::new(
                "terminal Memory encoder inputs do not match the frozen intent",
            ));
        }
        terminal_memory_materialization_reference(intent)
    }
}

impl MemoryMaterializationReferenceEncoder<SessionResourceMaterializationIntent>
    for HttpMemoryMaterializationReferenceEncoder
{
    fn encode(
        &self,
        _workspace_id: &str,
        memory_store_id: &str,
        config_version: ConfigVersion,
        access: ResourceAccess,
        intent: &SessionResourceMaterializationIntent,
    ) -> Result<String, MemoryMaterializationReferenceError> {
        let awaken_session_contract::SessionResourceMaterializationTarget::Input(input) =
            intent.target()
        else {
            return Err(MemoryMaterializationReferenceError::new(
                "Session Resource materialization target is not an input",
            ));
        };
        let awaken_session_contract::ResolvedInputSource::MemoryStore {
            memory_store_id: expected_store,
            config,
        } = &input.source
        else {
            return Err(MemoryMaterializationReferenceError::new(
                "Session Resource materialization target is not a Memory input",
            ));
        };
        if expected_store.as_str() != memory_store_id
            || config.version != config_version
            || input.access != access
        {
            return Err(MemoryMaterializationReferenceError::new(
                "Session Resource Memory encoder inputs do not match the frozen intent",
            ));
        }
        let encoded = serde_json::to_string(intent)
            .map_err(|error| MemoryMaterializationReferenceError::new(error.to_string()))?;
        Ok(format!("{MEMORY_REFERENCE_V3_PREFIX}{encoded}"))
    }
}

/// Encode the private HTTP transport capability. Application and run-ingress
/// layers consume only the neutral encoder port, never this representation.
pub fn memory_materialization_reference(
    workspace_id: &str,
    memory_store_id: &str,
    config_version: ConfigVersion,
    access: ResourceAccess,
    claim: &RunClaim,
) -> Result<String, MemoryMaterializationReferenceError> {
    let encoded = serde_json::to_string(&RunMemoryMaterializationReferenceV1 {
        workspace_id: workspace_id.to_owned(),
        memory_store_id: memory_store_id.to_owned(),
        config_version,
        access,
        claim: claim.clone(),
    })
    .map_err(|error| MemoryMaterializationReferenceError::new(error.to_string()))?;
    Ok(format!("{MEMORY_REFERENCE_V1_PREFIX}{encoded}"))
}

/// Encode a terminal-only v2 capability. Workspace is intentionally absent
/// from this Worker-authored wire value; the Session root supplies it only
/// after matching the intent to its current Environment binding.
pub fn terminal_memory_materialization_reference(
    intent: &SessionTerminalMemoryIntent,
) -> Result<String, MemoryMaterializationReferenceError> {
    let encoded = serde_json::to_string(&TerminalMemoryMaterializationReferenceV2 {
        binding_id: intent.binding_id().clone(),
        config_version: intent.config_version(),
        access: intent.access(),
        materialization: intent.materialization().clone(),
        fence: ResourceOperationFence::Terminal(intent.effect().clone()),
    })
    .map_err(|error| MemoryMaterializationReferenceError::new(error.to_string()))?;
    Ok(format!("{MEMORY_REFERENCE_V2_PREFIX}{encoded}"))
}

fn parse_reference(
    reference: &str,
) -> Result<MemoryMaterializationReference, MemoryMaterializationReferenceError> {
    if let Some(encoded) = reference.strip_prefix(MEMORY_REFERENCE_V1_PREFIX) {
        return serde_json::from_str(encoded)
            .map(MemoryMaterializationReference::RunV1)
            .map_err(|error| MemoryMaterializationReferenceError::new(error.to_string()));
    }
    if let Some(encoded) = reference.strip_prefix(MEMORY_REFERENCE_V3_PREFIX) {
        return serde_json::from_str(encoded)
            .map(Box::new)
            .map(MemoryMaterializationReference::RealizationV3)
            .map_err(|error| MemoryMaterializationReferenceError::new(error.to_string()));
    }
    let encoded = reference
        .strip_prefix(MEMORY_REFERENCE_V2_PREFIX)
        .ok_or_else(|| {
            MemoryMaterializationReferenceError::new(
                "operation requires an exact Memory authority reference",
            )
        })?;
    let decoded = serde_json::from_str::<TerminalMemoryMaterializationReferenceV2>(encoded)
        .map_err(|error| MemoryMaterializationReferenceError::new(error.to_string()))?;
    let ResourceOperationFence::Terminal(effect) = decoded.fence else {
        return Err(MemoryMaterializationReferenceError::new(
            "Memory v2 references are terminal-only",
        ));
    };
    SessionTerminalMemoryIntent::try_from_untrusted_parts(
        decoded.binding_id,
        decoded.config_version,
        decoded.access,
        decoded.materialization,
        effect,
    )
    .map(Box::new)
    .map(MemoryMaterializationReference::TerminalV2)
    .map_err(|error| MemoryMaterializationReferenceError::new(error.to_string()))
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum MemoryOperation {
    PreserveCopyConflict {
        candidate: awaken_resource_contract::MemoryCopyConflict,
    },
    /// `{}` and not a bare unit: a unit variant of an internally tagged enum
    /// escapes the container's `deny_unknown_fields`, so a mis-tagged write
    /// would decode as a read and silently perform nothing.
    Snapshot {},
    Create {
        path: String,
        content: String,
    },
    UpdateHead {
        base: awaken_resource_contract::MemoryGeneration,
        content: String,
        target_path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_path: Option<String>,
    },
    DeleteIfMatch {
        path: String,
        base: awaken_resource_contract::MemoryGeneration,
    },
}

impl MemoryOperation {
    fn writes(&self) -> bool {
        !matches!(self, Self::Snapshot {})
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MemoryOperationRequest {
    reference: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    identity: Option<WorkerIdentity>,
    operation: MemoryOperation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum MemoryOperationResponse {
    CopyConflictPreserved {},
    Snapshot { heads: Vec<WireMemory> },
    Memory { memory: WireMemory },
    Deleted { deleted: bool },
    Error { error: MemoryWireError },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum MemoryWireError {
    NotFound { value: String },
    Conflict { current: WireMemory },
    PathConflict { path: String },
    InvalidPath { path: String },
    TooLarge {},
    AtCapacity {},
    Storage { message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireMemory {
    id: String,
    path: String,
    content_sha256: String,
    content_size: u64,
    version: u64,
    created_unix_nanos: String,
    updated_unix_nanos: String,
    content: Option<String>,
}

impl From<Memory> for WireMemory {
    fn from(memory: Memory) -> Self {
        Self {
            id: memory.id,
            path: memory.path,
            content_sha256: memory.content_sha256,
            content_size: memory.content_size,
            version: memory.version,
            created_unix_nanos: memory.created_unix_nanos.to_string(),
            updated_unix_nanos: memory.updated_unix_nanos.to_string(),
            content: memory.content,
        }
    }
}

impl TryFrom<WireMemory> for Memory {
    type Error = MemErr;

    fn try_from(memory: WireMemory) -> Result<Self, Self::Error> {
        Ok(Self {
            id: memory.id,
            path: memory.path,
            content_sha256: memory.content_sha256,
            content_size: memory.content_size,
            version: memory.version,
            created_unix_nanos: memory.created_unix_nanos.parse().map_err(|error| {
                MemErr::Storage(format!("invalid Memory created timestamp: {error}"))
            })?,
            updated_unix_nanos: memory.updated_unix_nanos.parse().map_err(|error| {
                MemErr::Storage(format!("invalid Memory updated timestamp: {error}"))
            })?,
            content: memory.content,
        })
    }
}

impl From<MemErr> for MemoryWireError {
    fn from(error: MemErr) -> Self {
        match error {
            MemErr::NotFound(value) => Self::NotFound { value },
            MemErr::Conflict { current } => Self::Conflict {
                current: (*current).into(),
            },
            MemErr::PathConflict(path) => Self::PathConflict { path },
            MemErr::InvalidPath(path) => Self::InvalidPath { path },
            MemErr::TooLarge => Self::TooLarge {},
            MemErr::AtCapacity => Self::AtCapacity {},
            MemErr::Storage(message) => Self::Storage { message },
        }
    }
}

impl From<MemoryWireError> for MemErr {
    fn from(error: MemoryWireError) -> Self {
        match error {
            MemoryWireError::NotFound { value } => Self::NotFound(value),
            MemoryWireError::Conflict { current } => match current.try_into() {
                Ok(current) => Self::Conflict {
                    current: Box::new(current),
                },
                Err(error) => error,
            },
            MemoryWireError::PathConflict { path } => Self::PathConflict(path),
            MemoryWireError::InvalidPath { path } => Self::InvalidPath(path),
            MemoryWireError::TooLarge {} => Self::TooLarge,
            MemoryWireError::AtCapacity {} => Self::AtCapacity,
            MemoryWireError::Storage { message } => Self::Storage(message),
        }
    }
}

/// Exact Memory data-plane dependencies used by the Coordinator/Resource side.
pub struct WorkerMemoryService {
    repository: Arc<dyn MemoryRepository>,
    validator: Arc<dyn LiveResourceBindingVerifier>,
    dispatch: Arc<dyn DispatchQueue>,
    authenticator: Arc<dyn WorkerRequestAuthenticator>,
    directory: Arc<dyn WorkerDirectory>,
    session_control: Option<Arc<dyn SessionRealizationControl>>,
}

impl WorkerMemoryService {
    #[must_use]
    pub fn new(
        repository: Arc<dyn MemoryRepository>,
        validator: Arc<dyn LiveResourceBindingVerifier>,
        dispatch: Arc<dyn DispatchQueue>,
        authenticator: Arc<dyn WorkerRequestAuthenticator>,
        directory: Arc<dyn WorkerDirectory>,
    ) -> Self {
        Self {
            repository,
            validator,
            dispatch,
            authenticator,
            directory,
            session_control: None,
        }
    }

    #[must_use]
    pub fn with_session_control(
        mut self,
        session_control: Arc<dyn SessionRealizationControl>,
    ) -> Self {
        self.session_control = Some(session_control);
        self
    }
}

/// Mount only the claim-fenced Memory snapshot/write-back boundary.
pub fn worker_memory_router(service: Arc<WorkerMemoryService>) -> Router {
    Router::new()
        .route(MEMORY_PATH, post(memory_operation))
        .route_layer(axum::middleware::from_fn_with_state(
            service.authenticator.clone(),
            authenticate_worker_request,
        ))
        .with_state(service)
}

fn manifest_allows(
    dispatch: &awaken_dispatch_contract::RunDispatch,
    reference: &RunMemoryMaterializationReferenceV1,
    writes: bool,
) -> bool {
    let scope_matches = dispatch
        .execution_scope
        .as_ref()
        .is_some_and(|scope| scope.0.0 == reference.workspace_id);
    let binding_matches = dispatch
        .session_resources
        .as_ref()
        .filter(|envelope| envelope.workspace_id == reference.workspace_id)
        .and_then(|envelope| envelope.decode_manifest().ok())
        .is_some_and(|manifest| {
            manifest.resources.inputs().iter().any(|input| {
                matches!(
                    &input.source,
                    awaken_session_contract::ResolvedInputSource::MemoryStore {
                        memory_store_id,
                        config,
                    } if memory_store_id.as_str() == reference.memory_store_id
                        && config.version == reference.config_version
                        && input.access == reference.access
                        && (!writes || input.access == ResourceAccess::ReadWrite)
                )
            })
        });
    scope_matches && binding_matches
}

/// Terminal operation cause/effect table:
///
/// | Rule | requested mutation | durable A evidence | Effect |
/// |---|---|---|---|
/// | T1 | snapshot | any | allow conflict/readback observation |
/// | T2 | create path | path absent from A | allow one create CAS |
/// | T3 | update id/base | exactly one A id+sha, no rename | allow CAS update |
/// | T4 | conditional delete | exact A path+id+sha | allow CAS delete |
/// | T5 | any mutation | mismatched/ambiguous A | reject before repository I/O |
fn terminal_operation_allowed(
    target: &SessionTerminalMemoryTarget,
    operation: &MemoryOperation,
) -> bool {
    let heads = &target.intent().materialization().heads;
    match operation {
        MemoryOperation::PreserveCopyConflict { candidate } => match &candidate.base {
            Some(base) => {
                heads
                    .iter()
                    .filter(|head| {
                        head.path == candidate.path
                            && head.id == base.id
                            && head.version == base.version
                            && head.content_sha256 == base.content_sha256
                    })
                    .count()
                    == 1
            }
            None => {
                candidate.content.is_some() && !heads.iter().any(|head| head.path == candidate.path)
            }
        },
        MemoryOperation::Snapshot {} => true,
        MemoryOperation::Create { path, .. } => !heads.iter().any(|head| head.path == *path),
        MemoryOperation::UpdateHead {
            base,
            target_path,
            expected_path,
            ..
        } => {
            target_path.is_none()
                && heads
                    .iter()
                    .filter(|head| {
                        head.id == base.memory_id
                            && head.version == base.version
                            && head.content_sha256 == base.content_sha256
                            && expected_path.as_ref().is_none_or(|path| *path == head.path)
                    })
                    .count()
                    == 1
        }
        MemoryOperation::DeleteIfMatch { path, base } => {
            heads
                .iter()
                .filter(|head| {
                    head.path == *path
                        && head.id == base.memory_id
                        && head.version == base.version
                        && head.content_sha256 == base.content_sha256
                })
                .count()
                == 1
        }
    }
}

async fn memory_operation(
    State(service): State<Arc<WorkerMemoryService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<MemoryOperationRequest>,
) -> axum::response::Response {
    let Ok(reference) = parse_reference(&request.reference) else {
        return StatusCode::FORBIDDEN.into_response();
    };
    // R1 Run v1 holds the exact dispatch epoch through Memory I/O. R2 terminal
    // v2 ignores any expired Run claim and lets an asserted terminal generation
    // reach Control after its original expiry. Control must still prove a live
    // current same-generation root before returning the exact target; Registry
    // binding and A-base admission follow before repository I/O.
    let now_unix_ms = unix_now_ms();
    let mut run_guard = None;
    let (workspace_id, memory_store_id, config_version, terminal_target) = match &reference {
        MemoryMaterializationReference::RunV1(reference) => {
            // Retention is a terminal-only disposition, not a Run-authorized
            // append surface. Realization v3 already rejects every write.
            if matches!(
                request.operation,
                MemoryOperation::PreserveCopyConflict { .. }
            ) {
                return StatusCode::FORBIDDEN.into_response();
            }
            if awaken_worker_transport_security::verify_claim_owner(
                Some(service.directory.as_ref()),
                &worker,
                request.identity.as_ref(),
                &reference.claim,
                now_unix_ms,
            )
            .await
            .is_err()
            {
                return StatusCode::FORBIDDEN.into_response();
            }
            let guard = match service.dispatch.lock_commit_epoch(&reference.claim).await {
                Ok(Some(guard)) if guard.is_live_at(now_unix_ms) => guard,
                Ok(_) => return StatusCode::CONFLICT.into_response(),
                Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
            };
            if !manifest_allows(guard.request(), reference, request.operation.writes()) {
                return StatusCode::FORBIDDEN.into_response();
            }
            let binding = (
                reference.workspace_id.clone(),
                reference.memory_store_id.clone(),
                reference.config_version,
                None,
            );
            run_guard = Some(guard);
            binding
        }
        MemoryMaterializationReference::TerminalV2(intent) => {
            let Some(identity) = request.identity.as_ref() else {
                return StatusCode::FORBIDDEN.into_response();
            };
            let effect = intent.effect();
            if let Some(status) = verify_session_worker_effect(
                service.directory.as_ref(),
                &worker,
                identity,
                &effect.lease,
                now_unix_ms,
                SessionWorkerEffectTemporalRule::SessionRoot,
            )
            .await
            .rejection_status()
            {
                return status.into_response();
            }
            let Some(control) = service.session_control.as_deref() else {
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            };
            let target = match control
                .authorize_terminal_memory_intent(intent.as_ref())
                .await
            {
                Ok(target) if target.intent() == intent.as_ref() => target,
                Ok(_) => return StatusCode::FORBIDDEN.into_response(),
                Err(awaken_session_contract::SessionRealizationControlFailure::Unavailable(_))
                | Err(awaken_session_contract::SessionRealizationControlFailure::Conflict) => {
                    return StatusCode::SERVICE_UNAVAILABLE.into_response();
                }
                Err(_) => return StatusCode::CONFLICT.into_response(),
            };
            if !terminal_operation_allowed(&target, &request.operation) {
                return StatusCode::FORBIDDEN.into_response();
            }
            (
                target.workspace_id().to_owned(),
                target.memory_store_id().to_owned(),
                target.config_version(),
                Some(target),
            )
        }
        MemoryMaterializationReference::RealizationV3(intent) => {
            let Some(identity) = request.identity.as_ref() else {
                return StatusCode::FORBIDDEN.into_response();
            };
            if request.operation.writes() {
                return StatusCode::FORBIDDEN.into_response();
            }
            if let Some(status) = verify_session_worker_effect(
                service.directory.as_ref(),
                &worker,
                identity,
                intent.lease(),
                now_unix_ms,
                SessionWorkerEffectTemporalRule::SessionRoot,
            )
            .await
            .rejection_status()
            {
                return status.into_response();
            }
            let Some(control) = service.session_control.as_deref() else {
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            };
            let workspace_id = match control
                .authorize_resource_materialization_intent(intent)
                .await
            {
                Ok(workspace_id) => workspace_id,
                Err(awaken_session_contract::SessionRealizationControlFailure::Unavailable(_))
                | Err(awaken_session_contract::SessionRealizationControlFailure::Conflict) => {
                    return StatusCode::SERVICE_UNAVAILABLE.into_response();
                }
                Err(awaken_session_contract::SessionRealizationControlFailure::StaleOwnership) => {
                    return StatusCode::CONFLICT.into_response();
                }
                Err(_) => return StatusCode::FORBIDDEN.into_response(),
            };
            let awaken_session_contract::SessionResourceMaterializationTarget::Input(input) =
                intent.target()
            else {
                return StatusCode::FORBIDDEN.into_response();
            };
            let awaken_session_contract::ResolvedInputSource::MemoryStore {
                memory_store_id,
                config,
            } = &input.source
            else {
                return StatusCode::FORBIDDEN.into_response();
            };
            (
                workspace_id,
                memory_store_id.to_string(),
                config.version,
                None,
            )
        }
    };
    if let Err(error) = service
        .validator
        .verify_memory_binding(&workspace_id, &memory_store_id, config_version)
        .await
    {
        return (
            StatusCode::CONFLICT,
            Json(MemoryOperationResponse::Error {
                error: MemoryWireError::Storage {
                    message: error.to_string(),
                },
            }),
        )
            .into_response();
    }

    let _run_guard = run_guard;
    let result = match request.operation {
        MemoryOperation::PreserveCopyConflict { candidate } => service
            .repository
            .preserve_copy_conflict(&memory_store_id, &candidate)
            .await
            .map(|()| MemoryOperationResponse::CopyConflictPreserved {}),
        MemoryOperation::Snapshot {} => service
            .repository
            .snapshot_heads(&memory_store_id)
            .await
            .map(|heads| MemoryOperationResponse::Snapshot {
                heads: heads.into_iter().map(Into::into).collect(),
            }),
        MemoryOperation::Create { path, content } => service
            .repository
            .create(&memory_store_id, &path, &content)
            .await
            .map(|memory| MemoryOperationResponse::Memory {
                memory: memory.into(),
            }),
        MemoryOperation::UpdateHead {
            base,
            content,
            target_path,
            expected_path,
        } => {
            // Terminal v2 already froze the original path. Derive it even for
            // older clients that omit expected_path; ID-only CAS cannot waive
            // the terminal path fence after a content-preserving rename.
            let frozen_path = terminal_target.as_ref().and_then(|target| {
                target
                    .intent()
                    .materialization()
                    .heads
                    .iter()
                    .find(|head| head.id == base.memory_id)
                    .map(|head| head.path.as_str())
            });
            let path = frozen_path.or(expected_path.as_deref());
            let updated = match (path, target_path.as_deref()) {
                (Some(path), None) => {
                    service
                        .repository
                        .update_materialized(&memory_store_id, path, &base, &content)
                        .await
                }
                (Some(_), Some(_)) => Err(MemErr::Storage(
                    "materialized Memory update cannot rename".into(),
                )),
                (None, target) => {
                    service
                        .repository
                        .update_head(&memory_store_id, &base, &content, target)
                        .await
                }
            };
            updated.map(|memory| MemoryOperationResponse::Memory {
                memory: memory.into(),
            })
        }
        MemoryOperation::DeleteIfMatch { path, base } => service
            .repository
            .delete_if_match(&memory_store_id, &path, &base)
            .await
            .map(|deleted| MemoryOperationResponse::Deleted { deleted }),
    }
    .unwrap_or_else(|error| MemoryOperationResponse::Error {
        error: error.into(),
    });
    (StatusCode::OK, Json(result)).into_response()
}

/// HTTP client for the atomic Memory snapshot operation.
#[derive(Clone)]
pub struct HttpMemorySnapshotSource {
    upstream: WorkerUpstream,
}

/// HTTP client for claim-fenced Memory CAS mutations.
#[derive(Clone)]
pub struct HttpMemoryWritebackClient {
    upstream: WorkerUpstream,
}

/// Existing `MemoryRepository` port projected through the two narrow network
/// adapters. History/redaction/purge deliberately remain unavailable.
pub struct HttpMemoryRepository {
    snapshots: HttpMemorySnapshotSource,
    writeback: HttpMemoryWritebackClient,
}

impl HttpMemoryRepository {
    #[must_use]
    pub fn new(upstream: WorkerUpstream) -> Self {
        Self {
            snapshots: HttpMemorySnapshotSource {
                upstream: upstream.clone(),
            },
            writeback: HttpMemoryWritebackClient { upstream },
        }
    }
}

async fn send_operation(
    upstream: &WorkerUpstream,
    reference: &str,
    operation: MemoryOperation,
) -> Result<MemoryOperationResponse, MemErr> {
    parse_reference(reference).map_err(|error| MemErr::Storage(error.to_string()))?;
    let request = upstream
        .http_client()
        .post(format!("{}{MEMORY_PATH}", upstream.base_url()))
        .json(&MemoryOperationRequest {
            reference: reference.to_owned(),
            identity: upstream.worker_identity().cloned(),
            operation,
        });
    let request = upstream
        .authorize_request("POST", MEMORY_PATH, request)
        .map_err(MemErr::Storage)?;
    let response = request
        .send()
        .await
        .map_err(|error| MemErr::Storage(error.to_string()))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| MemErr::Storage(error.to_string()))?;
    if status != StatusCode::OK {
        if let Ok(MemoryOperationResponse::Error { error }) =
            serde_json::from_str::<MemoryOperationResponse>(&body)
        {
            return Err(error.into());
        }
        return Err(MemErr::Storage(format!(
            "Memory authority returned HTTP {status}; body={body}"
        )));
    }
    let response = serde_json::from_str::<MemoryOperationResponse>(&body).map_err(|error| {
        MemErr::Storage(format!(
            "decode Memory authority response: {error}; body={body}"
        ))
    })?;
    match response {
        MemoryOperationResponse::Error { error } => Err(error.into()),
        response => Ok(response),
    }
}

impl HttpMemorySnapshotSource {
    async fn snapshot(&self, reference: &str) -> Result<Vec<Memory>, MemErr> {
        match send_operation(&self.upstream, reference, MemoryOperation::Snapshot {}).await? {
            MemoryOperationResponse::Snapshot { heads } => {
                heads.into_iter().map(TryInto::try_into).collect()
            }
            _ => Err(MemErr::Storage(
                "Memory snapshot response kind mismatch".into(),
            )),
        }
    }
}

impl HttpMemoryWritebackClient {
    async fn send_memory(
        &self,
        reference: &str,
        operation: MemoryOperation,
    ) -> Result<Memory, MemErr> {
        match self.send(reference, operation).await? {
            MemoryOperationResponse::Memory { memory } => memory.try_into(),
            _ => Err(MemErr::Storage(
                "Memory write response kind mismatch".into(),
            )),
        }
    }

    async fn send(
        &self,
        reference: &str,
        operation: MemoryOperation,
    ) -> Result<MemoryOperationResponse, MemErr> {
        send_operation(&self.upstream, reference, operation).await
    }
}

#[async_trait::async_trait]
impl MemoryRepository for HttpMemoryRepository {
    async fn preserve_copy_conflict(
        &self,
        store: &str,
        candidate: &awaken_resource_contract::MemoryCopyConflict,
    ) -> Result<(), MemErr> {
        match self
            .writeback
            .send(
                store,
                MemoryOperation::PreserveCopyConflict {
                    candidate: candidate.clone(),
                },
            )
            .await?
        {
            MemoryOperationResponse::CopyConflictPreserved {} => Ok(()),
            _ => Err(MemErr::Storage(
                "Memory copy retention response kind mismatch".into(),
            )),
        }
    }
    async fn snapshot_heads(&self, store: &str) -> Result<Vec<Memory>, MemErr> {
        self.snapshots.snapshot(store).await
    }

    async fn list(
        &self,
        store: &str,
        prefix: &str,
    ) -> Result<Vec<awaken_resource_contract::MemoryEntry>, MemErr> {
        let mut entries = self
            .snapshot_heads(store)
            .await?
            .into_iter()
            .filter(|memory| prefix.is_empty() || prefix == "/" || memory.path.starts_with(prefix))
            .map(|memory| awaken_resource_contract::MemoryEntry {
                id: memory.id,
                path: memory.path,
                content_sha256: memory.content_sha256,
                content_size: memory.content_size,
                version: memory.version,
                updated_unix_nanos: memory.updated_unix_nanos,
            })
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(entries)
    }

    async fn get_by_path(&self, store: &str, path: &str) -> Result<Option<Memory>, MemErr> {
        Ok(self
            .snapshot_heads(store)
            .await?
            .into_iter()
            .find(|memory| memory.path == path))
    }

    async fn create(&self, store: &str, path: &str, content: &str) -> Result<Memory, MemErr> {
        self.writeback
            .send_memory(
                store,
                MemoryOperation::Create {
                    path: path.into(),
                    content: content.into(),
                },
            )
            .await
    }

    async fn update_head(
        &self,
        store: &str,
        base: &awaken_resource_contract::MemoryGeneration,
        content: &str,
        target_path: Option<&str>,
    ) -> Result<Memory, MemErr> {
        self.writeback
            .send_memory(
                store,
                MemoryOperation::UpdateHead {
                    base: base.clone(),
                    content: content.into(),
                    target_path: target_path.map(str::to_owned),
                    expected_path: None,
                },
            )
            .await
    }

    async fn update_materialized(
        &self,
        store: &str,
        path: &str,
        base: &awaken_resource_contract::MemoryGeneration,
        content: &str,
    ) -> Result<Memory, MemErr> {
        self.writeback
            .send_memory(
                store,
                MemoryOperation::UpdateHead {
                    base: base.clone(),
                    content: content.into(),
                    target_path: None,
                    expected_path: Some(path.into()),
                },
            )
            .await
    }

    async fn rename(&self, _store: &str, _from: &str, _to: &str) -> Result<Memory, MemErr> {
        Err(MemErr::Storage(
            "remote Memory rename is outside the snapshot/CAS materialization boundary".into(),
        ))
    }

    async fn delete_by_path(&self, store: &str, path: &str) -> Result<(), MemErr> {
        let Some(current) = self.get_by_path(store, path).await? else {
            return Ok(());
        };
        if self
            .delete_if_match(store, path, &current.generation())
            .await?
        {
            Ok(())
        } else {
            Err(MemErr::Conflict {
                current: Box::new(current),
            })
        }
    }

    async fn delete_if_match(
        &self,
        store: &str,
        path: &str,
        base: &awaken_resource_contract::MemoryGeneration,
    ) -> Result<bool, MemErr> {
        match self
            .writeback
            .send(
                store,
                MemoryOperation::DeleteIfMatch {
                    path: path.into(),
                    base: base.clone(),
                },
            )
            .await?
        {
            MemoryOperationResponse::Deleted { deleted } => Ok(deleted),
            _ => Err(MemErr::Storage(
                "Memory conditional delete response kind mismatch".into(),
            )),
        }
    }

    async fn list_versions(
        &self,
        _store: &str,
    ) -> Result<Vec<awaken_resource_contract::MemoryVersion>, MemErr> {
        Err(MemErr::Storage(
            "Worker Memory boundary does not expose history".into(),
        ))
    }

    async fn redact_version(
        &self,
        _store: &str,
        _version_id: &str,
    ) -> Result<Option<awaken_resource_contract::MemoryVersion>, MemErr> {
        Err(MemErr::Storage(
            "Worker Memory boundary does not expose redaction".into(),
        ))
    }

    async fn purge_store(
        &self,
        _store: &str,
    ) -> Result<awaken_resource_contract::MemoryPurgeSummary, MemErr> {
        Err(MemErr::Storage(
            "Worker Memory boundary does not expose lifecycle purge".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_free_memory_operations_refuse_the_keys_they_do_not_model() {
        // Every enum here carries `deny_unknown_fields`, and until this test the
        // attribute governed only the struct variants: serde reads an internally
        // tagged *unit* variant's tag and discards the rest of the object. The
        // three payload-free discriminants were therefore the only shapes on this
        // transport that accepted arbitrary keys, which is the opposite of what
        // the attribute beside them says.
        //
        // Causes over one tagged object: C1 tag alone; C2 tag plus a key a
        // sibling variant models; C3 tag plus a key no variant models.
        // Effects: E1 decode, E2 refuse.
        // | Rule | tagged object                               | Effect |
        // | M1   | snapshot, C1                                | E1     |
        // | M2   | snapshot, C2 (`content`: a Create field)    | E2     |
        // | M3   | snapshot, C3                                | E2     |
        // | M4   | copy_conflict_preserved, C1                 | E1     |
        // | M5   | copy_conflict_preserved, C2 (`heads`)       | E2     |
        // | M6   | too_large / at_capacity, C1                 | E1     |
        // | M7   | too_large / at_capacity, C2 (`message`)     | E2     |
        let operation =
            |value: serde_json::Value| serde_json::from_value::<MemoryOperation>(value).is_ok();
        assert!(operation(serde_json::json!({"kind": "snapshot"})), "M1");
        assert!(
            !operation(serde_json::json!({"kind": "snapshot", "content": "write me"})),
            "M2: a write that reaches the read discriminant must fail, not silently no-op"
        );
        assert!(
            !operation(serde_json::json!({"kind": "snapshot", "future": true})),
            "M3"
        );

        let response = |value: serde_json::Value| {
            serde_json::from_value::<MemoryOperationResponse>(value).is_ok()
        };
        assert!(
            response(serde_json::json!({"kind": "copy_conflict_preserved"})),
            "M4"
        );
        assert!(
            !response(serde_json::json!({"kind": "copy_conflict_preserved", "heads": []})),
            "M5"
        );

        let wire_error =
            |value: serde_json::Value| serde_json::from_value::<MemoryWireError>(value).is_ok();
        for kind in ["too_large", "at_capacity"] {
            assert!(wire_error(serde_json::json!({"kind": kind})), "M6 {kind}");
            assert!(
                !wire_error(serde_json::json!({"kind": kind, "message": "detail"})),
                "M7 {kind}"
            );
        }

        // The closed form must not move the wire: each still encodes as the tag
        // alone, so an already-deployed peer keeps round-tripping.
        assert_eq!(
            serde_json::to_value(MemoryOperation::Snapshot {}).unwrap(),
            serde_json::json!({"kind": "snapshot"})
        );
        assert_eq!(
            serde_json::to_value(MemoryOperationResponse::CopyConflictPreserved {}).unwrap(),
            serde_json::json!({"kind": "copy_conflict_preserved"})
        );
        assert_eq!(
            serde_json::to_value(MemoryWireError::AtCapacity {}).unwrap(),
            serde_json::json!({"kind": "at_capacity"})
        );
    }

    #[test]
    fn memory_update_wire_preserves_optional_location_without_weakening_strictness() {
        // MP-W decision table: a complete generation is mandatory; absent
        // expected_path => id-following update, string => lossless Copy location,
        // null => absent, and malformed/unknown authority fields fail closed.
        let base_wire = serde_json::json!({
            "kind": "update_head",
            "base": {"memory_id": "head", "version": 7, "content_sha256": "base"},
            "content": "edit", "target_path": null
        });
        for expected in [
            None,
            Some(serde_json::Value::Null),
            Some(serde_json::json!("/note.md")),
        ] {
            let mut wire = base_wire.clone();
            if let Some(expected) = expected {
                wire["expected_path"] = expected;
            }
            let operation: MemoryOperation = serde_json::from_value(wire.clone()).unwrap();
            let MemoryOperation::UpdateHead { expected_path, .. } = &operation else {
                panic!("MP-W update kind must be preserved");
            };
            assert_eq!(
                expected_path.as_deref(),
                wire.get("expected_path").and_then(|v| v.as_str())
            );
            let encoded = serde_json::to_value(operation).unwrap();
            if wire
                .get("expected_path")
                .is_none_or(serde_json::Value::is_null)
            {
                assert_eq!(encoded, base_wire, "MP-W absent location stays absent");
            } else {
                assert_eq!(encoded, wire, "MP-W exact location roundtrip");
            }
        }
        for (field, value) in [
            ("expected_path", serde_json::json!(42)),
            ("bypass", serde_json::json!(true)),
        ] {
            let mut wire = base_wire.clone();
            wire[field] = value;
            assert!(
                serde_json::from_value::<MemoryOperation>(wire).is_err(),
                "MP-W reject"
            );
        }
    }

    fn terminal_intent() -> SessionTerminalMemoryIntent {
        let effect = awaken_session_contract::SessionTerminalCleanupEffect::new(
            awaken_session_contract::SessionCleanupCommand::new(
                "session-memory",
                "session-memory",
                "cleanup-root",
            ),
            awaken_session_contract::SessionRealizationLease {
                owner: "worker-memory".into(),
                runtime_incarnation: "worker-memory:incarnation:3".into(),
                epoch: 3,
                expires_at_unix_ms: u64::MAX,
            },
        );
        SessionTerminalMemoryIntent::try_from_untrusted_parts(
            awaken_resource_contract::BindingId::from("binding-memory"),
            ConfigVersion(7),
            ResourceAccess::ReadWrite,
            awaken_provisioning_contract::MemoryMaterializationEvidence::new(
                "memory",
                "/memory/work",
                vec![awaken_provisioning_contract::MemoryMaterializationHead {
                    path: "/note.md".into(),
                    id: "memory-note".into(),
                    version: 1,
                    content_sha256: "sha-a".into(),
                }],
            )
            .unwrap(),
            effect,
        )
        .unwrap()
    }

    #[test]
    fn memory_reference_is_strict_lossless_and_claim_bound() {
        // Wire cause/effect decision table: W1 exact v1 facts plus Run claim =>
        // lossless rolling-compatible capability; W2 exact v2 intent plus
        // terminal fence => lossless capability without caller Workspace or old
        // Run claim; W3 bad prefix, unknown field, Run-in-v2, or non-canonical
        // evidence => reject. The HTTP reference is transport only, never a
        // second Resource, Session, or dispatch authority.
        let claim = RunClaim {
            run_id: awaken_agent_contract::agent::run::Id("run-memory".into()),
            owner: "worker-memory".into(),
            epoch: 3,
        };
        let encoded = memory_materialization_reference(
            "workspace",
            "memory",
            ConfigVersion(7),
            ResourceAccess::ReadWrite,
            &claim,
        )
        .expect("W1 encode");
        let parsed = parse_reference(&encoded).expect("W1 decode");
        let MemoryMaterializationReference::RunV1(parsed) = parsed else {
            panic!("W1 must remain v1")
        };
        assert_eq!(parsed.claim, claim, "W1 claim");
        assert_eq!(parsed.config_version, ConfigVersion(7), "W1 version");

        let intent = terminal_intent();
        let terminal = terminal_memory_materialization_reference(&intent).expect("W2 encode");
        assert!(!terminal.contains("workspace"), "W2 root owns Workspace");
        assert!(!terminal.contains("run_id"), "W2 has no stale Run claim");
        let MemoryMaterializationReference::TerminalV2(parsed) =
            parse_reference(&terminal).expect("W2 decode")
        else {
            panic!("W2 must remain v2")
        };
        assert_eq!(parsed.as_ref(), &intent, "W2 lossless");

        assert!(parse_reference("invalid:{}").is_err(), "W3 bad prefix");

        let unknown = format!(
            "{},\"authority_bypass\":true}}",
            encoded.strip_suffix('}').expect("reference JSON object")
        );
        assert!(parse_reference(&unknown).is_err(), "W3 unknown field");

        let run_in_v2 = terminal.replacen("\"type\":\"terminal\"", "\"type\":\"run\"", 1);
        assert!(parse_reference(&run_in_v2).is_err(), "W3 Run-in-v2");
    }

    #[test]
    fn terminal_operation_kernel_is_closed_over_original_heads() {
        // Mutation cause/effect decision table: O1 snapshot => allow; O2 create
        // absent-from-A => allow, existing-in-A => deny; O3 update exact generation
        // without rename => allow, stale/rename => deny; O4 delete exact
        // path+generation => allow, any mismatch => deny. These rules are evaluated
        // before the canonical repository sees an operation.
        let intent = terminal_intent();
        let target =
            SessionTerminalMemoryTarget::from_authorized_root("workspace", intent).unwrap();
        // O5 retention authorization cross-product: path={original,new},
        // base={original,absent}, content={write,delete}. An original path
        // requires its original base for either operation; an absent path
        // allows only a new write. Domain validation remains repository-owned.
        for path in ["/note.md", "/new.md"] {
            for has_base in [false, true] {
                for write in [false, true] {
                    let candidate = awaken_resource_contract::MemoryCopyConflict {
                        path: path.into(),
                        base: has_base.then(|| awaken_resource_contract::MemoryCopyBase {
                            id: "memory-note".into(),
                            version: 1,
                            content_sha256: "sha-a".into(),
                        }),
                        content: write.then(|| "edit".into()),
                    };
                    assert_eq!(
                        terminal_operation_allowed(
                            &target,
                            &MemoryOperation::PreserveCopyConflict { candidate },
                        ),
                        if path == "/note.md" {
                            has_base
                        } else {
                            !has_base && write
                        },
                        "O5 {path} base={has_base} write={write}",
                    );
                }
            }
        }
        assert!(
            terminal_operation_allowed(&target, &MemoryOperation::Snapshot {}),
            "O1"
        );
        assert!(
            terminal_operation_allowed(
                &target,
                &MemoryOperation::Create {
                    path: "/new.md".into(),
                    content: "new".into(),
                },
            ),
            "O2 absent"
        );
        assert!(
            !terminal_operation_allowed(
                &target,
                &MemoryOperation::Create {
                    path: "/note.md".into(),
                    content: "replacement".into(),
                },
            ),
            "O2 existing"
        );
        assert!(
            terminal_operation_allowed(
                &target,
                &MemoryOperation::UpdateHead {
                    base: awaken_resource_contract::MemoryGeneration {
                        memory_id: "memory-note".into(),
                        version: 1,
                        content_sha256: "sha-a".into(),
                    },
                    content: "B".into(),
                    target_path: None,
                    expected_path: None,
                },
            ),
            "O3 exact"
        );
        for operation in [
            MemoryOperation::UpdateHead {
                base: awaken_resource_contract::MemoryGeneration {
                    memory_id: "memory-note".into(),
                    version: 1,
                    content_sha256: "sha-c".into(),
                },
                content: "B".into(),
                target_path: None,
                expected_path: None,
            },
            MemoryOperation::UpdateHead {
                base: awaken_resource_contract::MemoryGeneration {
                    memory_id: "memory-note".into(),
                    version: 1,
                    content_sha256: "sha-a".into(),
                },
                content: "B".into(),
                target_path: Some("/renamed.md".into()),
                expected_path: None,
            },
            MemoryOperation::UpdateHead {
                base: awaken_resource_contract::MemoryGeneration {
                    memory_id: "memory-note".into(),
                    version: 1,
                    content_sha256: "sha-a".into(),
                },
                content: "B".into(),
                target_path: None,
                expected_path: Some("/foreign.md".into()),
            },
        ] {
            assert!(!terminal_operation_allowed(&target, &operation), "O3 deny");
        }
        assert!(
            terminal_operation_allowed(
                &target,
                &MemoryOperation::DeleteIfMatch {
                    path: "/note.md".into(),
                    base: awaken_resource_contract::MemoryGeneration {
                        memory_id: "memory-note".into(),
                        version: 1,
                        content_sha256: "sha-a".into(),
                    },
                },
            ),
            "O4 exact"
        );
        assert!(
            !terminal_operation_allowed(
                &target,
                &MemoryOperation::DeleteIfMatch {
                    path: "/note.md".into(),
                    base: awaken_resource_contract::MemoryGeneration {
                        memory_id: "memory-note".into(),
                        version: 1,
                        content_sha256: "sha-c".into(),
                    },
                },
            ),
            "O4 mismatch"
        );
    }
}
