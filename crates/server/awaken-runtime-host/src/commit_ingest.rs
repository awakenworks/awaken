//! The write-plane half of the Control/Worker seam: a database-less Worker sends
//! one versioned [`CommitOperation`] under its current claim. The Control Node
//! validates the claim and applies the operation through the authoritative
//! [`OperationCoordinator`].
//!
//! Paired with the dispatch transport (control plane): a worker claims a run over
//! the dispatch transport, drives it, then commits its facts here.

use std::sync::Arc;

use axum::extract::{Extension, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use serde_json::{Value, json};

use awaken_agent_contract::thread::commit::coordinator::{
    Error as CommitError, OperationCoordinator,
};
use awaken_agent_contract::thread::commit::operation::{CommitOperation, CommitReceipt};
use awaken_agent_contract::thread::commit::staged::{CommitRecord, ThreadCommit};
use awaken_run_ingress::{
    ClaimedCommitCommand, ClaimedRunCommit, DispatchQueue, RunClaim, WorkerDirectory,
    WorkerIdentity, WorkerRequestAuthorizer, WorkerState, commit_payload_hash,
};

use crate::host::HostError;
use crate::host::SharedHost;
use crate::worker_http::respond;
use crate::worker_security::{VerifiedWorkerContext, WORKER_ID_HEADER, WorkerRequestAuthenticator};

#[async_trait::async_trait]
trait CommitApplier: Send + Sync {
    async fn apply_operation(&self, operation: CommitOperation)
    -> Result<CommitReceipt, HostError>;
}

struct CoordinatorCommitApplier(Arc<dyn OperationCoordinator>);

#[async_trait::async_trait]
impl CommitApplier for CoordinatorCommitApplier {
    async fn apply_operation(
        &self,
        operation: CommitOperation,
    ) -> Result<CommitReceipt, HostError> {
        self.0
            .commit_operation(operation)
            .await
            .map_err(|error| HostError::internal(error.to_string()))
    }
}

struct HostCommitApplier(Arc<SharedHost>);

#[async_trait::async_trait]
impl CommitApplier for HostCommitApplier {
    async fn apply_operation(
        &self,
        operation: CommitOperation,
    ) -> Result<CommitReceipt, HostError> {
        let thread = operation.commit.thread_id.0.clone();
        let ctx = self.0.ctx_for(&thread, None).await?;
        ctx.commit
            .commit_operation(operation)
            .await
            .map_err(|error| HostError::internal(error.to_string()))
    }
}

/// Explicit application service for the claim-fenced commit boundary.
///
/// The injected coordinator is the committed-truth authority used by the
/// embedding Control Node. The service never constructs a [`SharedHost`] or
/// selects a storage backend.
pub struct ClaimedCommitService {
    dispatch: Arc<dyn DispatchQueue>,
    applier: Arc<dyn CommitApplier>,
    directory: Arc<dyn WorkerDirectory>,
    authenticator: Arc<dyn WorkerRequestAuthenticator>,
}

impl ClaimedCommitService {
    #[must_use]
    pub fn new(
        dispatch: Arc<dyn DispatchQueue>,
        coordinator: Arc<dyn OperationCoordinator>,
        directory: Arc<dyn WorkerDirectory>,
        authenticator: Arc<dyn WorkerRequestAuthenticator>,
    ) -> Self {
        Self {
            dispatch,
            applier: Arc::new(CoordinatorCommitApplier(coordinator)),
            directory,
            authenticator,
        }
    }

    pub(crate) fn for_host(
        dispatch: Arc<dyn DispatchQueue>,
        host: Arc<SharedHost>,
        directory: Arc<dyn WorkerDirectory>,
        authenticator: Arc<dyn WorkerRequestAuthenticator>,
    ) -> Self {
        Self {
            dispatch,
            applier: Arc::new(HostCommitApplier(host)),
            directory,
            authenticator,
        }
    }
}

struct CommitIngestState {
    applier: Arc<dyn CommitApplier>,
    dispatch: Arc<dyn DispatchQueue>,
    authenticator: Arc<dyn WorkerRequestAuthenticator>,
    directory: Arc<dyn WorkerDirectory>,
}

/// Mount the injectable, registered-worker claimed-commit service.
///
/// This is the sole Worker commit surface. It accepts only a versioned
/// [`CommitOperation`] and requires an authenticated, live Worker incarnation.
pub fn claimed_commit_router(service: Arc<ClaimedCommitService>) -> Router {
    commit_ingest_router_from_parts(
        service.applier.clone(),
        service.dispatch.clone(),
        service.authenticator.clone(),
        service.directory.clone(),
    )
}

fn commit_ingest_router_from_parts(
    applier: Arc<dyn CommitApplier>,
    dispatch: Arc<dyn DispatchQueue>,
    authenticator: Arc<dyn WorkerRequestAuthenticator>,
    directory: Arc<dyn WorkerDirectory>,
) -> Router {
    let state = Arc::new(CommitIngestState {
        applier,
        dispatch,
        authenticator,
        directory,
    });
    Router::new()
        .route(
            "/v1/worker/commit-claimed",
            axum::routing::post(commit_claimed),
        )
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            authenticate_commit_worker,
        ))
        .with_state(state)
}

async fn authenticate_commit_worker(
    State(state): State<Arc<CommitIngestState>>,
    request: Request,
    next: Next,
) -> Response {
    let (parts, body) = request.into_parts();
    match state.authenticator.authenticate(&parts).await {
        Ok(worker) => {
            let mut request = Request::from_parts(parts, body);
            request.extensions_mut().insert(worker);
            next.run(request).await
        }
        Err(error) => unauthorized(error.to_string()).into_response(),
    }
}

#[derive(serde::Deserialize)]
struct ClaimedCommitRequest {
    claim: RunClaim,
    operation: CommitOperation,
    identity: WorkerIdentity,
}

/// Atomically validate a remote worker's claim and apply its ThreadCommit while
/// the dispatch authority guard is live. Reclaim/settle/cancel cannot enter the
/// store between the validation and the commit.
async fn commit_claimed(
    State(state): State<Arc<CommitIngestState>>,
    Extension(worker): Extension<VerifiedWorkerContext>,
    Json(request): Json<ClaimedCommitRequest>,
) -> (StatusCode, Json<Value>) {
    let identity = &request.identity;
    if worker.worker_id() != identity.worker_id {
        return unauthorized("authenticated worker identity does not match".to_string());
    }
    if worker.credential_id().is_some() && worker.identity() != Some(identity) {
        return unauthorized(
            "authenticated worker incarnation does not match request identity".to_string(),
        );
    }
    let current = match state.directory.current(&identity.worker_id).await {
        Ok(current) => current,
        Err(error) => return unauthorized(error.to_string()),
    };
    if !current.as_ref().is_some_and(|record| {
        &record.snapshot.identity == identity
            && record.snapshot.state != WorkerState::Dead
            && record.snapshot.expires_at_ms > unix_now_ms()
    }) || request.claim.owner != identity.lease_owner()
    {
        return unauthorized("worker incarnation does not own the claim".to_string());
    }
    let result = async {
        let guard = state
            .dispatch
            .lock_commit_epoch(&request.claim)
            .await
            .map_err(|error| HostError::internal(error.to_string()))?;
        let Some(_guard) = guard else {
            return Err(HostError::bad_request("run claim is stale"));
        };
        let expected_hash = commit_payload_hash(&request.operation.commit)
            .map_err(|error| HostError::bad_request(error.to_string()))?;
        if expected_hash != request.operation.payload_hash {
            return Err(HostError::bad_request(
                "commit operation payload hash does not match ThreadCommit",
            ));
        }
        let receipt = state.applier.apply_operation(request.operation).await?;
        Ok(serde_json::to_value(receipt).expect("CommitReceipt serializes"))
    }
    .await;
    respond(result)
}

fn unauthorized(message: String) -> (StatusCode, Json<Value>) {
    (StatusCode::UNAUTHORIZED, Json(json!({ "error": message })))
}

/// Atomic claimed-run commit used by a database-less worker. Unlike
/// an ordinary coordinator, this sends the claim and commit operation together.
pub struct RemoteClaimedRunCommit {
    base_url: String,
    client: reqwest::Client,
    identity: WorkerIdentity,
    request_authorizer: Option<Arc<dyn WorkerRequestAuthorizer>>,
}

const CLAIMED_COMMIT_TRANSPORT_ATTEMPTS: usize = 3;
const CLAIMED_COMMIT_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(25);

impl RemoteClaimedRunCommit {
    pub fn new(base_url: impl Into<String>, identity: WorkerIdentity) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client: reqwest::Client::new(),
            identity,
            request_authorizer: None,
        }
    }

    /// Use a caller-configured client (for example one carrying a worker mTLS
    /// identity) instead of the default client.
    #[must_use]
    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }

    #[must_use]
    pub fn with_request_authorizer(mut self, authorizer: Arc<dyn WorkerRequestAuthorizer>) -> Self {
        self.request_authorizer = Some(authorizer.bind_worker_identity(&self.identity));
        self
    }

    fn authorize(
        &self,
        path: &str,
        worker_id: &str,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder, CommitError> {
        match &self.request_authorizer {
            Some(authorizer) => authorizer
                .authorize("POST", path, worker_id, request)
                .map_err(CommitError::Rejected),
            None => Ok(request.header(WORKER_ID_HEADER, worker_id)),
        }
    }
}

#[async_trait::async_trait]
impl ClaimedRunCommit for RemoteClaimedRunCommit {
    async fn commit(
        &self,
        _claim: &RunClaim,
        _commit: ThreadCommit,
    ) -> Result<CommitRecord, CommitError> {
        Err(CommitError::Rejected(
            "remote Worker commits require a versioned CommitOperation".to_string(),
        ))
    }

    async fn commit_operation(
        &self,
        command: ClaimedCommitCommand,
    ) -> Result<CommitReceipt, CommitError> {
        let path = "/v1/worker/commit-claimed";
        let body = json!({
            "claim": command.claim,
            "operation": command.operation,
            "identity": &self.identity
        });
        let mut last_transport_error = None;
        for attempt in 1..=CLAIMED_COMMIT_TRANSPORT_ATTEMPTS {
            let request = self.client.post(format!("{}{path}", self.base_url));
            let response = match self
                .authorize(path, &self.identity.worker_id, request)?
                .json(&body)
                .send()
                .await
            {
                Ok(response) => response,
                Err(error) => {
                    last_transport_error = Some(format!("claimed commit transport: {error}"));
                    if attempt < CLAIMED_COMMIT_TRANSPORT_ATTEMPTS {
                        tokio::time::sleep(CLAIMED_COMMIT_RETRY_DELAY).await;
                        continue;
                    }
                    break;
                }
            };
            if !response.status().is_success() {
                return Err(CommitError::Rejected(format!(
                    "claimed commit server returned {}",
                    response.status()
                )));
            }
            match response.json().await {
                Ok(receipt) => return Ok(receipt),
                Err(error) => {
                    last_transport_error =
                        Some(format!("commit receipt transport/decode: {error}"));
                    if attempt < CLAIMED_COMMIT_TRANSPORT_ATTEMPTS {
                        tokio::time::sleep(CLAIMED_COMMIT_RETRY_DELAY).await;
                    }
                }
            }
        }
        Err(CommitError::Rejected(last_transport_error.unwrap_or_else(
            || "claimed commit transport exhausted without a response".to_string(),
        )))
    }
}

pub(crate) fn remote_claimed_commit(
    upstream: &crate::worker_security::WorkerUpstream,
) -> Result<Arc<dyn ClaimedRunCommit>, HostError> {
    let identity = upstream.worker_identity().cloned().ok_or_else(|| {
        HostError::internal("remote Worker commit transport requires a registered identity")
    })?;
    Ok(Arc::new(
        RemoteClaimedRunCommit::new(upstream.base_url(), identity)
            .with_client(upstream.client().clone())
            .with_request_authorizer(upstream.request_authorizer()),
    ))
}

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}
