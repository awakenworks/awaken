//! Claim-fenced Memory snapshot and CAS write-back adapters.
//!
//! This is a Memory-specific Worker boundary. It carries one frozen
//! Workspace/store/config/access binding and delegates to the canonical
//! [`awaken_memory_store::MemoryRepository`]. It exposes no store authoring,
//! history redaction, retention, or lifecycle operation.

use std::sync::Arc;

use awaken_memory_store::{MemErr, Memory, MemoryRepository};
use awaken_resource_contract::{ConfigVersion, ResourceAccess, ResourceBindingValidator};
use awaken_run_ingress::{DispatchQueue, RunClaim, WorkerDirectory, WorkerIdentity};
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use awaken_worker_transport_security::{
    VerifiedWorkerContext, WorkerRequestAuthenticator, WorkerUpstream, authenticate_worker_request,
};

const MEMORY_PATH: &str = "/v1/worker/resources/memory/operation";
const REFERENCE_PREFIX: &str = "awaken-memory-v1:";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MemoryMaterializationReference {
    workspace_id: String,
    memory_store_id: String,
    config_version: ConfigVersion,
    access: ResourceAccess,
    claim: RunClaim,
}

/// Create the process-local projection reference carried by a Memory mount.
/// The reference is not a Resource identity and is never persisted in the
/// Session manifest; it binds every Worker read/write to the exact claim.
pub fn memory_materialization_reference(
    workspace_id: &str,
    memory_store_id: &str,
    config_version: ConfigVersion,
    access: ResourceAccess,
    claim: &RunClaim,
) -> Result<String, MemErr> {
    let encoded = serde_json::to_string(&MemoryMaterializationReference {
        workspace_id: workspace_id.to_owned(),
        memory_store_id: memory_store_id.to_owned(),
        config_version,
        access,
        claim: claim.clone(),
    })
    .map_err(|error| MemErr::Storage(error.to_string()))?;
    Ok(format!("{REFERENCE_PREFIX}{encoded}"))
}

fn parse_reference(reference: &str) -> Result<MemoryMaterializationReference, MemErr> {
    let encoded = reference.strip_prefix(REFERENCE_PREFIX).ok_or_else(|| {
        MemErr::Storage("Memory operation requires an exact materialization reference".into())
    })?;
    serde_json::from_str(encoded).map_err(|error| MemErr::Storage(error.to_string()))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum MemoryOperation {
    Snapshot,
    Create {
        path: String,
        content: String,
    },
    UpdateHead {
        id: String,
        content: String,
        base_sha: String,
        target_path: Option<String>,
    },
    DeleteIfMatch {
        path: String,
        base_id: String,
        base_sha: String,
    },
}

impl MemoryOperation {
    fn writes(&self) -> bool {
        !matches!(self, Self::Snapshot)
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
    TooLarge,
    AtCapacity,
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
            MemErr::TooLarge => Self::TooLarge,
            MemErr::AtCapacity => Self::AtCapacity,
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
            MemoryWireError::TooLarge => Self::TooLarge,
            MemoryWireError::AtCapacity => Self::AtCapacity,
            MemoryWireError::Storage { message } => Self::Storage(message),
        }
    }
}

/// Exact Memory data-plane dependencies used by the Coordinator/Resource side.
pub struct WorkerMemoryService {
    repository: Arc<dyn MemoryRepository>,
    validator: Arc<dyn ResourceBindingValidator>,
    dispatch: Arc<dyn DispatchQueue>,
    authenticator: Arc<dyn WorkerRequestAuthenticator>,
    directory: Arc<dyn WorkerDirectory>,
}

impl WorkerMemoryService {
    #[must_use]
    pub fn new(
        repository: Arc<dyn MemoryRepository>,
        validator: Arc<dyn ResourceBindingValidator>,
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
        }
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

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

fn manifest_allows(
    dispatch: &awaken_run_ingress::RunDispatch,
    reference: &MemoryMaterializationReference,
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
        .and_then(|envelope| crate::provisioning::decode_session_resource_envelope(envelope).ok())
        .is_some_and(|manifest| {
            manifest.resources.inputs.iter().any(|input| {
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

async fn memory_operation(
    State(service): State<Arc<WorkerMemoryService>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<MemoryOperationRequest>,
) -> axum::response::Response {
    let Ok(reference) = parse_reference(&request.reference) else {
        return StatusCode::FORBIDDEN.into_response();
    };
    if awaken_worker_transport_security::verify_claim_owner(
        Some(service.directory.as_ref()),
        &worker,
        request.identity.as_ref(),
        &reference.claim,
        unix_now_ms(),
    )
    .await
    .is_err()
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    let guard = match service.dispatch.lock_commit_epoch(&reference.claim).await {
        Ok(Some(guard)) if guard.is_live_at(unix_now_ms()) => guard,
        Ok(_) => return StatusCode::CONFLICT.into_response(),
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    if !manifest_allows(guard.request(), &reference, request.operation.writes()) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if service
        .validator
        .validate_memory_binding(
            &reference.workspace_id,
            &reference.memory_store_id,
            reference.config_version,
        )
        .is_err()
    {
        return StatusCode::CONFLICT.into_response();
    }

    let result = match request.operation {
        MemoryOperation::Snapshot => service
            .repository
            .snapshot_heads(&reference.memory_store_id)
            .await
            .map(|heads| MemoryOperationResponse::Snapshot {
                heads: heads.into_iter().map(Into::into).collect(),
            }),
        MemoryOperation::Create { path, content } => service
            .repository
            .create(&reference.memory_store_id, &path, &content)
            .await
            .map(|memory| MemoryOperationResponse::Memory {
                memory: memory.into(),
            }),
        MemoryOperation::UpdateHead {
            id,
            content,
            base_sha,
            target_path,
        } => service
            .repository
            .update_head(
                &reference.memory_store_id,
                &id,
                &content,
                &base_sha,
                target_path.as_deref(),
            )
            .await
            .map(|memory| MemoryOperationResponse::Memory {
                memory: memory.into(),
            }),
        MemoryOperation::DeleteIfMatch {
            path,
            base_id,
            base_sha,
        } => service
            .repository
            .delete_if_match(&reference.memory_store_id, &path, &base_id, &base_sha)
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
    parse_reference(reference)?;
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
    if response.status() != StatusCode::OK {
        return Err(MemErr::Storage(format!(
            "Memory authority returned HTTP {}",
            response.status()
        )));
    }
    let body = response
        .text()
        .await
        .map_err(|error| MemErr::Storage(error.to_string()))?;
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
        match send_operation(&self.upstream, reference, MemoryOperation::Snapshot).await? {
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
    async fn snapshot_heads(&self, store: &str) -> Result<Vec<Memory>, MemErr> {
        self.snapshots.snapshot(store).await
    }

    async fn list(
        &self,
        store: &str,
        prefix: &str,
    ) -> Result<Vec<awaken_memory_store::MemoryEntry>, MemErr> {
        let mut entries = self
            .snapshot_heads(store)
            .await?
            .into_iter()
            .filter(|memory| prefix.is_empty() || prefix == "/" || memory.path.starts_with(prefix))
            .map(|memory| awaken_memory_store::MemoryEntry {
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
        match self
            .writeback
            .send(
                store,
                MemoryOperation::Create {
                    path: path.into(),
                    content: content.into(),
                },
            )
            .await?
        {
            MemoryOperationResponse::Memory { memory } => memory.try_into(),
            _ => Err(MemErr::Storage(
                "Memory create response kind mismatch".into(),
            )),
        }
    }

    async fn update_head(
        &self,
        store: &str,
        id: &str,
        content: &str,
        base_sha: &str,
        target_path: Option<&str>,
    ) -> Result<Memory, MemErr> {
        match self
            .writeback
            .send(
                store,
                MemoryOperation::UpdateHead {
                    id: id.into(),
                    content: content.into(),
                    base_sha: base_sha.into(),
                    target_path: target_path.map(str::to_owned),
                },
            )
            .await?
        {
            MemoryOperationResponse::Memory { memory } => memory.try_into(),
            _ => Err(MemErr::Storage(
                "Memory update response kind mismatch".into(),
            )),
        }
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
            .delete_if_match(store, path, &current.id, &current.content_sha256)
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
        base_id: &str,
        base_sha: &str,
    ) -> Result<bool, MemErr> {
        match self
            .writeback
            .send(
                store,
                MemoryOperation::DeleteIfMatch {
                    path: path.into(),
                    base_id: base_id.into(),
                    base_sha: base_sha.into(),
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
    ) -> Result<Vec<awaken_memory_store::MemoryVersion>, MemErr> {
        Err(MemErr::Storage(
            "Worker Memory boundary does not expose history".into(),
        ))
    }

    async fn redact_version(
        &self,
        _store: &str,
        _version_id: &str,
    ) -> Result<Option<awaken_memory_store::MemoryVersion>, MemErr> {
        Err(MemErr::Storage(
            "Worker Memory boundary does not expose redaction".into(),
        ))
    }

    async fn purge_store(
        &self,
        _store: &str,
    ) -> Result<awaken_memory_store::MemoryPurgeSummary, MemErr> {
        Err(MemErr::Storage(
            "Worker Memory boundary does not expose lifecycle purge".into(),
        ))
    }
}
