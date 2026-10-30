// @formal surface obligation=observation_reconcile.evidence_aba_has_fresh_fence
// @formal surface obligation=worker_credential.stale_heartbeat_never_rolls_back_revision
// @formal surface obligation=sandbox.admission_never_weakens_isolation
// @formal reason: Authoritative Worker reads and deny-only bootstrap enrollment CAS.
//! Authenticated private Worker observations and bootstrap admission closure.
//!
//! Worker registration and heartbeat authority remains the Coordinator-owned
//! [`WorkerDirectory`](awaken_worker_registry::WorkerDirectory). Observation readers
//! receive only a secret-free projection. Bootstrap closure requires its separate
//! permission and can only seal an original, unregistered provider enrollment;
//! it cannot create a Registry incarnation, process exit or Session cleanup receipt.

use awaken_session_application::SessionRootReads as _;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use awaken_worker_registry::{
    RegisteredWorker, RegistryError, WorkerDirectory, WorkerObservationSource,
};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

pub const WORKER_OBSERVATIONS_PATH: &str = "/internal/v1/workers/observations";
pub const WORKER_OBSERVE_PERMISSION: &str = "worker:observe";
pub(crate) const WORKER_BOOTSTRAP_CLOSE_PATH: &str = "/internal/v1/workers/bootstrap/close";
const WORKER_BOOTSTRAP_CLOSE_PERMISSION: &str = "worker:close-bootstrap";
pub const WORKER_DOMAIN_PATH: &str = "/internal/v1/workers/domain";
pub use awaken_worker_contract::WORKER_RELAY_DESTINATION_PATH;
#[cfg(all(test, feature = "test-support"))]
mod occupancy_tests;
#[cfg(all(test, feature = "test-support"))]
mod relay_tests;
/// Total per-attempt deadline for one private control-plane call.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const ATTEMPTS: usize = 3;
const RETRY_DELAY: Duration = Duration::from_millis(25);

#[derive(Clone)]
struct BoundaryState {
    source: Arc<dyn WorkerObservationSource>,
    authenticator: Arc<dyn awaken_service_auth_contract::ServiceRequestAuthenticator>,
}

#[derive(Clone)]
pub(crate) struct DomainState {
    pub(crate) directory: Arc<dyn WorkerDirectory>,
    /// The durable dispatch authority whose physical attempts the read judges.
    pub(crate) dispatch: Arc<awaken_dispatch_erasure::AnyCoordinatorDispatch>,
    /// Incarnation-qualified occupancy from the original WorkQueue authority.
    pub(crate) work_claims: Arc<dyn awaken_executable_environment_contract::WorkerLeaseObservation>,
    pub(crate) session_queries: Arc<awaken_session_application::SessionQueries>,
    pub(crate) worker_occupancy: Arc<awaken_session_application::SessionWorkerOccupancy>,
    pub(crate) relay_targets: Option<Arc<dyn awaken_worker_contract::WorkerRelayTargetObserver>>,
    pub(crate) bindings: Option<Arc<dyn awaken_dispatch::WorkerExecutionBindingSource>>,
    pub(crate) clock: Arc<dyn awaken_time::WallClock>,
    pub(crate) authenticator: Arc<dyn awaken_service_auth_contract::ServiceRequestAuthenticator>,
    pub(crate) relay_authenticator:
        Option<Arc<dyn awaken_service_auth_contract::ServiceRequestAuthenticator>>,
    /// The last refusal explanation logged per exact Worker incarnation, so a
    /// Worker polling its own retirement logs only when the reasons change.
    pub(crate) retirement_explanations: RetirementExplanations,
}

/// Process-local memory of logged retirement refusals. It is diagnostics only:
/// it never participates in the retirement decision.
#[derive(Clone, Default)]
pub(crate) struct RetirementExplanations(
    Arc<
        std::sync::Mutex<
            std::collections::BTreeMap<
                String,
                Vec<awaken_session_application::WorkerRetirementBlocker>,
            >,
        >,
    >,
);

impl RetirementExplanations {
    /// Record `blockers` for `worker`; true when they differ from the last record.
    fn changed(
        &self,
        worker: &str,
        blockers: &[awaken_session_application::WorkerRetirementBlocker],
    ) -> bool {
        let mut last = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if last
            .get(worker)
            .is_some_and(|previous| previous == blockers)
        {
            return false;
        }
        if blockers.is_empty() {
            last.remove(worker);
        } else {
            last.insert(worker.to_owned(), blockers.to_vec());
        }
        true
    }
}

/// A read-only composition of the original Registry, Dispatch, and WorkQueue
/// owners. Neither this DTO nor its route can close or retire an attempt.
#[derive(Debug, Serialize, Deserialize)]
pub struct WorkerDomainObservation {
    pub registered: RegisteredWorker,
    pub physical_attempt_active: bool,
    pub work_claims_active: bool,
    pub session_realizations_active: bool,
    /// Present only for explicit release qualification. The original four
    /// facts retain their meaning; pending cleanup is never projected as absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retirement_permitted: Option<bool>,
}

/// Transient read decision; never persisted or accepted as an exit/cleanup receipt.
#[derive(Debug, PartialEq, Eq)]
enum WorkerRetirementBasis {
    Blocked,
    Empty,
    RetainedSources,
}

impl WorkerDomainObservation {
    fn retirement_basis(&self) -> WorkerRetirementBasis {
        use awaken_worker_contract::WorkerState;
        let registered = &self.registered;
        if self.physical_attempt_active
            || self.work_claims_active
            || registered.snapshot.in_flight != 0
            || registered.create_ledger.is_none()
            || !matches!(
                registered.snapshot.state,
                WorkerState::Draining | WorkerState::Quiesced | WorkerState::Dead
            )
        {
            return WorkerRetirementBasis::Blocked;
        }
        if !self.session_realizations_active {
            return WorkerRetirementBasis::Empty;
        }
        WorkerRetirementBasis::RetainedSources
    }
}

// Partial read of the same query consumed by canonical WorkerIdentity. Its
// identity fields must remain with that owner; this is never a request body.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct WorkerRetirementQuery {
    #[serde(default)]
    pub qualify_retirement: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerRelayQuery {
    session_id: String,
    attachment_id: String,
    version: u64,
}

pub(crate) fn domain_router_with_authenticator(state: DomainState) -> Router {
    Router::new()
        .route(WORKER_DOMAIN_PATH, get(handle_domain))
        .route(WORKER_BOOTSTRAP_CLOSE_PATH, post(handle_bootstrap_close))
        .route(WORKER_RELAY_DESTINATION_PATH, get(handle_relay_destination))
        .with_state(state)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BootstrapCloseRequest {
    worker_id: String,
    create_ledger: awaken_worker_contract::WorkerCreateLedger,
}

#[derive(Serialize)]
struct BootstrapCloseObservation {
    worker_id: String,
    create_ledger: awaken_worker_contract::WorkerCreateLedger,
}

fn validate_bootstrap_request(
    worker_id: &str,
    expected: &awaken_worker_contract::WorkerCreateLedger,
) -> Result<(), StatusCode> {
    if worker_id.trim().is_empty() || worker_id.trim() != worker_id || worker_id.len() > 1024 {
        return Err(StatusCode::BAD_REQUEST);
    }
    expected.validate().map_err(|_| StatusCode::BAD_REQUEST)
}

async fn handle_bootstrap_close(
    State(state): State<DomainState>,
    headers: HeaderMap,
    Json(request): Json<BootstrapCloseRequest>,
) -> axum::response::Response {
    let requirement = awaken_service_auth_contract::ServiceAuthorizationRequirement::new(
        awaken_service_auth_contract::COORDINATOR_SERVICE_AUDIENCE,
        WORKER_BOOTSTRAP_CLOSE_PERMISSION,
    );
    if let Err(error) = state.authenticator.authenticate(
        headers.get(header::AUTHORIZATION).map(|h| h.as_bytes()),
        requirement,
    ) {
        return match error {
            awaken_service_auth_contract::ServiceAuthError::Unauthorized => {
                StatusCode::UNAUTHORIZED
            }
            awaken_service_auth_contract::ServiceAuthError::Forbidden => StatusCode::FORBIDDEN,
            _ => StatusCode::SERVICE_UNAVAILABLE,
        }
        .into_response();
    }
    if let Err(status) = validate_bootstrap_request(&request.worker_id, &request.create_ledger) {
        return status.into_response();
    }
    match state
        .worker_occupancy
        .observe_worker_realizations(
            awaken_session_application::WorkerRealizationRead::Occupancy(&request.worker_id),
        )
        .await
    {
        Ok(observed) if !observed.occupied => {}
        Ok(_) => return StatusCode::CONFLICT.into_response(),
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
    match close_bootstrap(
        state.directory.as_ref(),
        state.bindings.as_deref(),
        request.worker_id,
        request.create_ledger,
    )
    .await
    {
        Ok(observation) => match state
            .worker_occupancy
            .observe_worker_realizations(
                awaken_session_application::WorkerRealizationRead::Occupancy(
                    &observation.worker_id,
                ),
            )
            .await
        {
            Ok(observed) if !observed.occupied => {
                (StatusCode::OK, Json(observation)).into_response()
            }
            Ok(_) => StatusCode::CONFLICT.into_response(),
            Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
        },
        Err(status) => status.into_response(),
    }
}

async fn close_bootstrap(
    directory: &dyn WorkerDirectory,
    bindings: Option<&dyn awaken_dispatch::WorkerExecutionBindingSource>,
    worker_id: String,
    expected: awaken_worker_contract::WorkerCreateLedger,
) -> Result<BootstrapCloseObservation, StatusCode> {
    validate_bootstrap_request(&worker_id, &expected)?;
    match directory.current(&worker_id).await {
        Ok(None) => {}
        Ok(Some(_)) => return Err(StatusCode::CONFLICT),
        Err(_) => return Err(StatusCode::SERVICE_UNAVAILABLE),
    }
    let bindings = bindings.ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let anchor = bindings
        .seal_unattempted_worker(&worker_id, &expected)
        .await
        .map_err(|_| StatusCode::CONFLICT)?;
    if anchor != expected {
        return Err(StatusCode::CONFLICT);
    }
    // First binding and seal share provider CAS. A registration that already
    // passed that boundary retains a binding and cannot produce this result.
    // Recheck the original Registry after the deny fence; never invent an
    // incarnation or turn an unavailable observation into absence.
    match directory.current(&worker_id).await {
        Ok(None) => Ok(BootstrapCloseObservation {
            worker_id,
            create_ledger: anchor,
        }),
        Ok(Some(_)) => Err(StatusCode::CONFLICT),
        Err(_) => Err(StatusCode::SERVICE_UNAVAILABLE),
    }
}

async fn handle_relay_destination(
    State(state): State<DomainState>,
    headers: HeaderMap,
    query: Result<Query<WorkerRelayQuery>, axum::extract::rejection::QueryRejection>,
) -> axum::response::Response {
    let Some(authenticator) = state.relay_authenticator.as_ref() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    if let Err(status) = authenticate_observer(authenticator.as_ref(), &headers) {
        return status.into_response();
    }
    let query = match query {
        Ok(Query(query))
            if !query.session_id.is_empty()
                && query.session_id.len() <= 512
                && query.session_id.trim() == query.session_id
                && !query.attachment_id.is_empty()
                && query.attachment_id.len() <= 512
                && query.attachment_id.trim() == query.attachment_id
                && query.version > 0 =>
        {
            query
        }
        _ => return StatusCode::BAD_REQUEST.into_response(),
    };
    let Some(targets) = &state.relay_targets else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let now = awaken_time::WallClockExt::now_ms(state.clock.as_ref());
    let session_id = awaken_session_types::SessionId::from(query.session_id);
    let session = match state.session_queries.session(&session_id).await {
        Ok(session) => session,
        Err(awaken_session_contract::SessionRepositoryError::NotFound) => {
            return StatusCode::NOT_FOUND.into_response();
        }
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let Some((worker_id, generation)) = session.current_worker_relay_generation(
        &awaken_session_configuration_contract::mcp::McpAttachmentId(query.attachment_id),
        query.version,
        now,
    ) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let registered = match state.directory.current(&worker_id).await {
        Ok(Some(registered))
            if registered.snapshot.identity.worker_id == worker_id
                && registered.snapshot.state.accepts_work()
                && registered.snapshot.expires_at_ms > now
                && registered.process_exit.is_none() =>
        {
            registered
        }
        Ok(_) => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let ip = match targets.ip_for(&registered.snapshot.identity).await {
        Ok(ip) => ip,
        Err(awaken_worker_contract::WorkerRelayTargetError::InvalidIdentity) => {
            return StatusCode::NOT_FOUND.into_response();
        }
        Err(awaken_worker_contract::WorkerRelayTargetError::Unavailable) => {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };
    let expiry = generation
        .lease_expires_at_unix_ms()
        .min(registered.snapshot.expires_at_ms);
    match awaken_worker_contract::WorkerRelayDestination::new(
        registered.snapshot.identity,
        ip,
        expiry,
    ) {
        Ok(destination) => (StatusCode::OK, Json(destination)).into_response(),
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

fn authenticate_observer(
    authenticator: &dyn awaken_service_auth_contract::ServiceRequestAuthenticator,
    headers: &HeaderMap,
) -> Result<(), StatusCode> {
    match authenticator.authenticate(
        headers
            .get(header::AUTHORIZATION)
            .map(|value| value.as_bytes()),
        awaken_service_auth_contract::ServiceAuthorizationRequirement::new(
            awaken_service_auth_contract::COORDINATOR_SERVICE_AUDIENCE,
            WORKER_OBSERVE_PERMISSION,
        ),
    ) {
        Ok(_) => Ok(()),
        Err(awaken_service_auth_contract::ServiceAuthError::Unauthorized) => {
            Err(StatusCode::UNAUTHORIZED)
        }
        Err(awaken_service_auth_contract::ServiceAuthError::Forbidden) => {
            Err(StatusCode::FORBIDDEN)
        }
        Err(awaken_service_auth_contract::ServiceAuthError::Unavailable(_)) => {
            Err(StatusCode::SERVICE_UNAVAILABLE)
        }
    }
}

/// The authenticated observation boundary exposes classification and exact
/// context, never operator details (which may contain provider credentials).
#[derive(Debug, Serialize, Deserialize)]
pub struct WorkerDomainReadFailure {
    pub identity: awaken_worker_registry::WorkerIdentity,
    pub stage: String,
    pub kind: String,
    pub code: String,
}

fn domain_read_failure(
    identity: &awaken_worker_registry::WorkerIdentity,
    stage: &'static str,
    error: awaken_session_types::RunError,
) -> axum::response::Response {
    use awaken_session_types::RunErrorKind;
    let (status, kind) = match error.kind {
        RunErrorKind::BadRequest => (StatusCode::BAD_REQUEST, "bad_request"),
        RunErrorKind::Conflict => (StatusCode::CONFLICT, "conflict"),
        RunErrorKind::Internal => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
        RunErrorKind::Unavailable => (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
    };
    tracing::warn!(
        worker_id = %identity.worker_id,
        incarnation_id = %identity.incarnation_id,
        generation = identity.generation,
        stage,
        kind,
        code = %error.code,
        "Worker domain observation failed"
    );
    (
        status,
        Json(WorkerDomainReadFailure {
            identity: identity.clone(),
            stage: stage.into(),
            kind: kind.into(),
            code: error.code,
        }),
    )
        .into_response()
}

async fn handle_domain(
    State(state): State<DomainState>,
    headers: HeaderMap,
    query: Result<
        Query<awaken_worker_registry::WorkerIdentity>,
        axum::extract::rejection::QueryRejection,
    >,
    retirement: Result<Query<WorkerRetirementQuery>, axum::extract::rejection::QueryRejection>,
) -> axum::response::Response {
    if let Err(status) = authenticate_observer(state.authenticator.as_ref(), &headers) {
        return status.into_response();
    }
    let identity = match query {
        Ok(Query(identity)) => identity,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    let retirement = match retirement {
        Ok(Query(query)) => query.qualify_retirement,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    if identity.worker_id.trim().is_empty()
        || identity.incarnation_id.trim().is_empty()
        || identity.generation == 0
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let registered = match state.directory.incarnation(&identity).await {
        Ok(Some(record)) if record.snapshot.identity == identity => record,
        Ok(Some(_)) => {
            return domain_read_failure(
                &identity,
                "registry",
                awaken_session_types::RunError::unavailable_classified(
                    "worker_identity_changed",
                    "Registry identity changed",
                ),
            );
        }
        Ok(None) => {
            return domain_read_failure(
                &identity,
                "registry",
                awaken_session_types::RunError::conflict_classified(
                    "worker_not_registered",
                    "Worker is not registered",
                ),
            );
        }
        Err(error) => {
            return domain_read_failure(
                &identity,
                "registry",
                awaken_session_types::RunError::unavailable_classified(
                    "worker_registry_unavailable",
                    error.to_string(),
                ),
            );
        }
    };
    let dispatch = state.dispatch.clone();
    let owner = identity.lease_owner();
    let physical_attempt_active =
        match awaken_dispatch::has_active_physical_attempt_for_owner(dispatch.as_ref(), &owner)
            .await
        {
            Ok(active) => active,
            Err(error) => {
                return domain_read_failure(
                    &identity,
                    "physical_attempts",
                    awaken_session_types::RunError::unavailable_classified(
                        "worker_physical_read_unavailable",
                        error.to_string(),
                    ),
                );
            }
        };
    let work_claims_active = match state.work_claims.has_owner_claims(&owner).await {
        Ok(active) => active,
        Err(error) => {
            return domain_read_failure(
                &identity,
                "work_claims",
                awaken_session_types::RunError::unavailable_classified(
                    "worker_work_read_unavailable",
                    error.to_string(),
                ),
            );
        }
    };
    let mut observation = WorkerDomainObservation {
        registered,
        physical_attempt_active,
        work_claims_active,
        session_realizations_active: true,
        retirement_permitted: None,
    };
    // Select the exact same complete Session read once. Retirement decides
    // from the retention this exact incarnation reported; the Coordinator
    // observes no Sandbox and a missing or stale report refuses.
    let read =
        if retirement && observation.retirement_basis() == WorkerRetirementBasis::RetainedSources {
            awaken_session_application::WorkerRealizationRead::Retirement {
                identity: &identity,
                reported: &observation.registered.source_retention,
            }
        } else {
            awaken_session_application::WorkerRealizationRead::Occupancy(&identity.worker_id)
        };
    let sessions = match state
        .worker_occupancy
        .observe_worker_realizations(read)
        .await
    {
        Ok(observed) => observed,
        Err(error) => return domain_read_failure(&identity, "session_realizations", error),
    };
    observation.session_realizations_active = sessions.occupied;
    if retirement {
        observation.retirement_permitted = Some(match observation.retirement_basis() {
            WorkerRetirementBasis::Blocked => false,
            WorkerRetirementBasis::Empty => true,
            WorkerRetirementBasis::RetainedSources => sessions.sources_retained,
        });
        if observation.retirement_basis() == WorkerRetirementBasis::RetainedSources {
            explain_retirement(&state, &identity, &observation).await;
        }
    }
    (StatusCode::OK, Json(observation)).into_response()
}

/// Log which Sessions refuse a retainable Worker's retirement and why. Only a
/// change is logged at info; the read is the decision's own complete read and
/// a failure to explain never changes the response.
async fn explain_retirement(
    state: &DomainState,
    identity: &awaken_worker_contract::WorkerIdentity,
    observation: &WorkerDomainObservation,
) {
    let worker = format!("{}:{}", identity.worker_id, identity.lease_owner());
    match state
        .worker_occupancy
        .explain_worker_retirement(identity, &observation.registered.source_retention)
        .await
    {
        Ok(blockers) => {
            if !state.retirement_explanations.changed(&worker, &blockers) {
                return;
            }
            if blockers.is_empty() {
                tracing::info!(%worker, "Worker retirement is no longer refused by any Session");
            } else {
                let sessions = blockers
                    .iter()
                    .map(|blocker| format!("{}={:?}", blocker.session_id, blocker.reason))
                    .collect::<Vec<_>>()
                    .join(", ");
                tracing::info!(
                    %worker,
                    refused_by = blockers.len(),
                    %sessions,
                    "Worker retirement refused by Sessions"
                );
            }
        }
        Err(error) => tracing::debug!(%worker, %error, "Worker retirement explanation unavailable"),
    }
}

pub fn router(
    source: Arc<dyn WorkerObservationSource>,
    bearer_token: impl Into<String>,
) -> Result<Router, String> {
    Ok(router_with_authenticator(
        source,
        awaken_service_auth_contract::static_token_authenticator(bearer_token)?,
    ))
}

pub fn router_with_authenticator(
    source: Arc<dyn WorkerObservationSource>,
    authenticator: Arc<dyn awaken_service_auth_contract::ServiceRequestAuthenticator>,
) -> Router {
    Router::new()
        .route(WORKER_OBSERVATIONS_PATH, get(handle_list))
        .with_state(BoundaryState {
            source,
            authenticator,
        })
}

async fn handle_list(
    State(state): State<BoundaryState>,
    headers: HeaderMap,
) -> axum::response::Response {
    if let Err(status) = authenticate_observer(state.authenticator.as_ref(), &headers) {
        return status.into_response();
    }
    match state.source.list().await {
        Ok(workers) => (StatusCode::OK, Json(Ok::<_, String>(workers))).into_response(),
        Err(error) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(Err::<Vec<RegisteredWorker>, _>(error.to_string())),
        )
            .into_response(),
    }
}

/// Read-only remote adapter used by a split Control process.
/// Authenticated roster and exact-incarnation observations from one Coordinator.
/// R8: the exact-domain adapter does not implement the logical-current role.
/// Its capability refusal is checked before any HTTP request, not by a hidden
/// unsupported implementation or a guessed current row from the roster.
///
/// ```compile_fail
/// use awaken_coordinator::worker_observation_boundary::HttpWorkerObservationSource;
/// use awaken_worker_contract::WorkerCurrentSource;
/// fn requires_current<T: WorkerCurrentSource>() {}
/// requires_current::<HttpWorkerObservationSource>();
/// ```
#[derive(Clone)]
pub struct HttpWorkerObservationSource {
    endpoint: String,
    domain_endpoint: String,
    token_source: Arc<dyn awaken_service_auth_contract::ServiceBearerTokenSource>,
    client: awaken_outbound_http::InternalHttpClient,
}

impl HttpWorkerObservationSource {
    pub fn new(
        base_url: impl Into<String>,
        bearer_token: impl Into<String>,
    ) -> Result<Self, String> {
        Self::with_token_source(
            base_url,
            awaken_service_auth_contract::static_token_source(bearer_token)?,
        )
    }

    pub fn with_token_source(
        base_url: impl Into<String>,
        token_source: Arc<dyn awaken_service_auth_contract::ServiceBearerTokenSource>,
    ) -> Result<Self, String> {
        let base_url = awaken_outbound_http::normalize_http_base_url(&base_url.into()).map_err(
            |rejected| match rejected {
                awaken_outbound_http::BaseUrlRejected::Malformed => {
                    format!("invalid Coordinator observation URL: {rejected}")
                }
                _ => "Coordinator observations require an http(s) base URL and non-empty bearer \
                      token"
                    .to_owned(),
            },
        )?;
        awaken_service_auth_contract::resolve_service_bearer_token(token_source.as_ref())?;
        Ok(Self {
            endpoint: format!("{base_url}{WORKER_OBSERVATIONS_PATH}"),
            domain_endpoint: format!("{base_url}{WORKER_DOMAIN_PATH}"),
            token_source,
            client: awaken_outbound_http::InternalHttpClient::new(REQUEST_TIMEOUT)
                .map_err(|error| error.to_string())?,
        })
    }

    /// The original authenticated GET/retry boundary shared by both read
    /// projections. Only the exact domain endpoint interprets 409 as absence.
    async fn get_observation(
        &self,
        endpoint: &str,
        missing_incarnation_is_none: bool,
    ) -> Result<Option<awaken_outbound_http::HttpReply>, RegistryError> {
        let mut last_error = "Coordinator Worker-observation service unavailable".to_owned();
        for attempt in 0..ATTEMPTS {
            let bearer_token = awaken_service_auth_contract::resolve_service_bearer_token(
                self.token_source.as_ref(),
            )
            .map_err(RegistryError::Persistence)?;
            match self
                .client
                .send(
                    &awaken_outbound_http::JsonRequest::get(endpoint).bearer(bearer_token.as_ref()),
                )
                .await
            {
                Ok(response) if response.is_success() => {
                    return Ok(Some(response));
                }
                Ok(response) if missing_incarnation_is_none && response.status() == 409 => {
                    return Ok(None);
                }
                Ok(response) if response.is_unauthorized() => {
                    let rotated = awaken_service_auth_contract::service_bearer_token_rotated(
                        self.token_source.as_ref(),
                        bearer_token.as_ref(),
                    )
                    .map_err(RegistryError::Persistence)?;
                    if attempt + 1 < ATTEMPTS && rotated {
                        continue;
                    }
                    return Err(RegistryError::Persistence(
                        "Coordinator Worker-observation credentials were rejected".into(),
                    ));
                }
                Ok(response) if response.is_client_error() => {
                    return Err(RegistryError::Persistence(format!(
                        "Coordinator Worker-observation request rejected: {}",
                        response.status()
                    )));
                }
                Ok(response) => {
                    last_error = format!(
                        "Coordinator Worker-observation service returned {}",
                        response.status()
                    );
                }
                Err(error) => {
                    last_error = format!("call Coordinator Worker-observation service: {error}");
                }
            }
            if attempt + 1 < ATTEMPTS {
                tokio::time::sleep(RETRY_DELAY).await;
            }
        }
        Err(RegistryError::Persistence(last_error))
    }
}

#[async_trait]
impl WorkerObservationSource for HttpWorkerObservationSource {
    async fn list(&self) -> Result<Vec<RegisteredWorker>, RegistryError> {
        let response = self
            .get_observation(&self.endpoint, false)
            .await?
            .ok_or_else(|| {
                RegistryError::Persistence("Coordinator Worker observations are missing".into())
            })?;
        serde_json::from_str::<Result<Vec<RegisteredWorker>, String>>(response.body())
            .map_err(|error| {
                RegistryError::Persistence(format!(
                    "decode Coordinator Worker observations: {error}"
                ))
            })?
            .map_err(RegistryError::Persistence)
    }
}

#[async_trait::async_trait]
impl awaken_worker_contract::WorkerIncarnationSource for HttpWorkerObservationSource {
    async fn incarnation(
        &self,
        identity: &awaken_worker_registry::WorkerIdentity,
    ) -> Result<Option<RegisteredWorker>, RegistryError> {
        let mut endpoint = url::Url::parse(&self.domain_endpoint).map_err(|error| {
            RegistryError::Persistence(format!("Coordinator Worker domain URL: {error}"))
        })?;
        endpoint
            .query_pairs_mut()
            .append_pair("worker_id", &identity.worker_id)
            .append_pair("incarnation_id", &identity.incarnation_id)
            .append_pair("generation", &identity.generation.to_string());
        let Some(response) = self.get_observation(endpoint.as_str(), true).await? else {
            return Ok(None);
        };
        if response.status() != 200 {
            return Err(RegistryError::Persistence(format!(
                "Coordinator Worker domain did not return an exact read: {}",
                response.status()
            )));
        }
        let observation: WorkerDomainObservation =
            serde_json::from_str(response.body()).map_err(|error| {
                RegistryError::Persistence(format!("decode Coordinator Worker domain: {error}"))
            })?;
        if observation.registered.snapshot.identity != *identity {
            return Err(RegistryError::Persistence(
                "Coordinator Worker domain returned another incarnation".into(),
            ));
        }
        Ok(Some(observation.registered))
    }
}

#[cfg(test)]
mod tests {
    use awaken_worker_contract::WorkerCurrentSource;
    use awaken_worker_contract::WorkerIncarnationSource;
    use awaken_worker_registry::WorkerObservationSource;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    use super::*;

    #[tokio::test]
    async fn domain_read_errors_preserve_fault_class_and_scope_without_operator_details() {
        // DR3 decision table: all four neutral fault kinds x misleading/ordinary
        // codes x the four actual read stages (the dispatch owner is a
        // construction-time port, so it has no read stage). HTTP follows kind, never code
        // prose; the original code and exact Worker identity survive. DR4 a
        // large sensitive operator message is absent from the bounded JSON and
        // no failure can expose a successful retirement permission field.
        let identity = awaken_worker_registry::WorkerIdentity::new("worker-a", "boot-a", 7);
        for stage in [
            "registry",
            "physical_attempts",
            "work_claims",
            "session_realizations",
        ] {
            for (error, status, kind, code) in [
                (
                    awaken_session_types::RunError::bad_request("private-detail".repeat(8192)),
                    StatusCode::BAD_REQUEST,
                    "bad_request",
                    "invalid_request",
                ),
                (
                    awaken_session_types::RunError::bad_request_classified(
                        "unavailable",
                        "private-detail",
                    ),
                    StatusCode::BAD_REQUEST,
                    "bad_request",
                    "unavailable",
                ),
                (
                    awaken_session_types::RunError::conflict("private-detail"),
                    StatusCode::CONFLICT,
                    "conflict",
                    "conflict",
                ),
                (
                    awaken_session_types::RunError::conflict_classified(
                        "internal",
                        "private-detail",
                    ),
                    StatusCode::CONFLICT,
                    "conflict",
                    "internal",
                ),
                (
                    awaken_session_types::RunError::internal("private-detail"),
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal",
                    "internal",
                ),
                (
                    awaken_session_types::RunError::classified(
                        "environment_unavailable",
                        "private-detail",
                    ),
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal",
                    "environment_unavailable",
                ),
                (
                    awaken_session_types::RunError::unavailable("private-detail"),
                    StatusCode::SERVICE_UNAVAILABLE,
                    "unavailable",
                    "unavailable",
                ),
                (
                    awaken_session_types::RunError::unavailable_classified(
                        "environment_rejected",
                        "private-detail",
                    ),
                    StatusCode::SERVICE_UNAVAILABLE,
                    "unavailable",
                    "environment_rejected",
                ),
            ] {
                let response = domain_read_failure(&identity, stage, error);
                assert_eq!(response.status(), status, "DR3 {stage}/{kind}/{code}");
                let body = axum::body::to_bytes(response.into_body(), 4096)
                    .await
                    .unwrap();
                assert!(
                    !String::from_utf8_lossy(&body).contains("private-detail"),
                    "DR4"
                );
                let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(
                    body,
                    serde_json::json!({"identity":identity,"stage":stage,"kind":kind,"code":code})
                );
                let exported: WorkerDomainReadFailure =
                    serde_json::from_value(body.clone()).unwrap();
                assert_eq!(
                    serde_json::to_value(exported).unwrap(),
                    body,
                    "DR3 exported wire contract"
                );
            }
        }
    }

    pub(super) struct BootstrapBinding {
        pub(super) calls: std::sync::atomic::AtomicUsize,
        pub(super) mode: u8,
        pub(super) directory: Arc<awaken_worker_registry::MemoryWorkerDirectory>,
    }

    #[async_trait]
    impl awaken_dispatch::WorkerExecutionBindingSource for BootstrapBinding {
        async fn seal_unattempted_worker(
            &self,
            worker_id: &str,
            _expected: &awaken_dispatch::WorkerCreateLedger,
        ) -> Result<
            awaken_dispatch::WorkerCreateLedger,
            awaken_session_execution_contract::ApplicationError,
        > {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.mode == 1 {
                return Err(
                    awaken_session_execution_contract::ApplicationError::unavailable(
                        "ledger unknown",
                    ),
                );
            }
            if self.mode == 2 {
                self.directory
                    .register(
                        awaken_worker_contract::WorkerRegistration {
                            worker_id: worker_id.into(),
                            incarnation_id: "racing-registration".into(),
                            manifest: Default::default(),
                            create_ledger: None,
                        },
                        1,
                        100,
                    )
                    .await
                    .unwrap();
            }
            awaken_worker_contract::WorkerCreateLedger::new(
                "provider",
                "control",
                "original-ledger",
                if self.mode == 3 {
                    "replaced-uid"
                } else {
                    "original-uid"
                },
                "original-config",
            )
            .map_err(|e| {
                awaken_session_execution_contract::ApplicationError::internal(e.to_string())
            })
        }
        async fn binding_for(
            &self,
            _: &awaken_dispatch::WorkerIdentity,
        ) -> Result<
            awaken_dispatch::AttemptExecutionBinding,
            awaken_session_execution_contract::ApplicationError,
        > {
            unreachable!("bootstrap never invents an incarnation")
        }
    }

    #[tokio::test]
    async fn bootstrap_close_requires_original_deny_fence_and_two_authoritative_registry_reads() {
        // C1 no original registration; C2 provider can seal the exact existing
        // unbound fresh ledger; C3 second Registry read is still absent.
        // R1 C1+C2+C3 -> original anchor, no fake incarnation; R2 !C1 -> no
        // provider mutation; R3 !C2/missing provider -> refusal; R4 registration
        // races provider response -> refusal. Absence alone never succeeds.
        for cause in [
            "empty",
            "registered",
            "unknown-ledger",
            "changed-anchor",
            "race",
            "no-provider",
            "invalid-id",
        ] {
            let directory = Arc::new(awaken_worker_registry::MemoryWorkerDirectory::new());
            let id = "k8s:ns:worker:uid";
            if cause == "registered" {
                directory
                    .register(
                        awaken_worker_contract::WorkerRegistration {
                            worker_id: id.into(),
                            incarnation_id: "original".into(),
                            manifest: Default::default(),
                            create_ledger: None,
                        },
                        1,
                        100,
                    )
                    .await
                    .unwrap();
            }
            let binding = BootstrapBinding {
                calls: 0.into(),
                mode: match cause {
                    "unknown-ledger" => 1,
                    "race" => 2,
                    "changed-anchor" => 3,
                    _ => 0,
                },
                directory: directory.clone(),
            };
            let result = close_bootstrap(
                directory.as_ref(),
                (cause != "no-provider")
                    .then_some(&binding as &dyn awaken_dispatch::WorkerExecutionBindingSource),
                if cause == "invalid-id" {
                    " ".into()
                } else {
                    id.into()
                },
                awaken_worker_contract::WorkerCreateLedger::new(
                    "provider",
                    "control",
                    "original-ledger",
                    "original-uid",
                    "original-config",
                )
                .unwrap(),
            )
            .await;
            assert_eq!(result.is_ok(), cause == "empty", "{cause}");
            assert_eq!(
                binding.calls.load(std::sync::atomic::Ordering::SeqCst),
                usize::from(matches!(
                    cause,
                    "empty" | "unknown-ledger" | "race" | "changed-anchor"
                )),
                "{cause} early refusal"
            );
            if let Ok(observed) = result {
                assert_eq!(observed.worker_id, id);
                assert_eq!(observed.create_ledger.uid(), "original-uid");
                assert!(
                    directory.current(id).await.unwrap().is_none(),
                    "R1 no Registry fabrication"
                );
            }
        }
    }

    struct FixedSource;

    struct UnavailableAuthenticator;

    impl awaken_service_auth_contract::ServiceRequestAuthenticator for UnavailableAuthenticator {
        fn authenticate(
            &self,
            _authorization: Option<&[u8]>,
            _requirement: awaken_service_auth_contract::ServiceAuthorizationRequirement<'_>,
        ) -> Result<
            awaken_service_auth_contract::AuthenticatedService,
            awaken_service_auth_contract::ServiceAuthError,
        > {
            Err(awaken_service_auth_contract::ServiceAuthError::Unavailable(
                "token projection unavailable".into(),
            ))
        }
    }

    #[async_trait]
    impl WorkerObservationSource for FixedSource {
        async fn list(&self) -> Result<Vec<RegisteredWorker>, RegistryError> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn retirement_qualification_keeps_every_existing_execution_blocker() {
        // Q1 exact closing Actor + no Run/Work/in-flight activity + original anchor
        // asks the independent source owner. Its cleanup lease may still be live:
        // expiry alone neither permits stop nor proves source retention. Q2 every
        // activity/anchor/phase blocker denies the read. Q3 vacant sources retain
        // the same Native decision owner, without a provider dependency.
        use awaken_worker_contract::WorkerState::*;
        for (rule, phase, physical, work, in_flight, anchor, expected) in [
            ("draining-live", Draining, false, false, 0, true, true),
            ("draining-expired", Draining, false, false, 0, true, true),
            ("quiesced", Quiesced, false, false, 0, true, true),
            ("dead", Dead, false, false, 0, true, true),
            ("ready", Ready, false, false, 0, true, false),
            ("starting", Starting, false, false, 0, true, false),
            ("physical", Draining, true, false, 0, true, false),
            ("work", Draining, false, true, 0, true, false),
            ("in-flight", Draining, false, false, 1, true, false),
            ("missing-anchor", Draining, false, false, 0, false, false),
        ] {
            let mut registered = historical_worker(awaken_worker_contract::WorkerIdentity::new(
                "worker", "boot", 1,
            ));
            registered.snapshot.state = phase;
            registered.snapshot.in_flight = in_flight;
            registered.snapshot.expires_at_ms = if rule == "draining-live" { u64::MAX } else { 1 };
            if !anchor {
                registered.create_ledger = None;
            }
            let observation = WorkerDomainObservation {
                registered,
                physical_attempt_active: physical,
                work_claims_active: work,
                session_realizations_active: true,
                retirement_permitted: None,
            };
            assert_eq!(
                observation.retirement_basis(),
                if expected {
                    WorkerRetirementBasis::RetainedSources
                } else {
                    WorkerRetirementBasis::Blocked
                },
                "{rule}"
            );
            let wire = serde_json::to_value(&observation).unwrap();
            assert_eq!(wire["session_realizations_active"], true);
            assert!(
                wire.get("retirement_permitted").is_none(),
                "source proof still required"
            );
            let mut empty = observation;
            empty.session_realizations_active = false;
            assert_eq!(
                empty.retirement_basis(),
                if expected {
                    WorkerRetirementBasis::Empty
                } else {
                    WorkerRetirementBasis::Blocked
                }
            );
        }
    }

    #[test]
    fn retirement_source_qualification_is_an_explicit_read_projection() {
        // Q3 old exact-incarnation reads retain their original URI and four
        // facts; provider availability cannot suppress their exit observation.
        // Q4 the explicit selector changes no identity field; malformed
        // selectors fail closed. Both extractors use the production Axum parser.
        // The selector deliberately accepts identity keys owned by the other
        // extractor, plus future read-only query keys; it never decodes a body.
        let base =
            format!("{WORKER_DOMAIN_PATH}?worker_id=worker&incarnation_id=boot&generation=1");
        for (suffix, requested) in [
            ("", false),
            ("&qualify_retirement=false", false),
            ("&qualify_retirement=true", true),
            ("&future_read_projection=ignored", false),
        ] {
            let uri = format!("{base}{suffix}").parse().unwrap();
            let identity = Query::<awaken_worker_contract::WorkerIdentity>::try_from_uri(&uri)
                .unwrap()
                .0;
            assert_eq!(
                identity,
                awaken_worker_contract::WorkerIdentity::new("worker", "boot", 1)
            );
            assert_eq!(
                Query::<WorkerRetirementQuery>::try_from_uri(&uri)
                    .unwrap()
                    .0
                    .qualify_retirement,
                requested
            );
        }
        let malformed = format!("{base}&qualify_retirement=unknown")
            .parse()
            .unwrap();
        assert!(Query::<WorkerRetirementQuery>::try_from_uri(&malformed).is_err());
    }

    fn historical_worker(identity: awaken_worker_registry::WorkerIdentity) -> RegisteredWorker {
        let manifest = awaken_worker_contract::WorkerManifest::default();
        RegisteredWorker {
            snapshot: awaken_worker_contract::WorkerSnapshot {
                identity,
                state: awaken_worker_contract::WorkerState::Dead,
                capability_fingerprint: manifest.fingerprint().unwrap(),
                manifest,
                in_flight: 0,
                warm_environment_shapes: Default::default(),
                credential_observations: Default::default(),
                acp_capability_observations: Default::default(),
                expires_at_ms: 5,
            },
            heartbeat_sequence: 1,
            observation_sequence: 1,
            registered_at_ms: 1,
            heartbeat_at_ms: 2,
            drain_deadline_ms: None,
            process_exit: Some(
                awaken_worker_contract::WorkerProcessExit::new("original", "process", "exit")
                    .unwrap(),
            ),
            create_ledger: Some(
                awaken_worker_contract::WorkerCreateLedger::new(
                    "original",
                    "control",
                    "ledger",
                    "original-uid",
                    "configuration",
                )
                .unwrap(),
            ),
            source_retention: Default::default(),
        }
    }

    #[tokio::test]
    async fn historical_http_read_preserves_exact_identity_wire_and_failure_kinds() {
        // R6 old exact row, including original exit/ledger and true domain
        // flags -> return that row only, with no closure decision. R7 a 200
        // mismatched identity/missing field/bad JSON or non-200 success (202/204)
        // cannot qualify. R8 only domain409 is None; list409/400/401 refuse.
        // R9 503 retries exactly the existing bound. R10 reserved/Unicode
        // identity values round-trip through the actual HTTP query decoder.
        // No path falls back to list or mutates the Registry.
        use std::sync::atomic::{AtomicUsize, Ordering};
        let identity = awaken_worker_registry::WorkerIdentity::new(
            "worker:space &+?#%/雪",
            "boot &+?#%/雪",
            2,
        );
        let original = historical_worker(identity.clone());
        for rule in [
            "exact",
            "identity",
            "field",
            "bad-json",
            "accepted",
            "no-content",
            "missing",
            "bad-query",
            "unauthorized",
            "unavailable",
        ] {
            let domain_calls = Arc::new(AtomicUsize::new(0));
            let list_calls = Arc::new(AtomicUsize::new(0));
            let app = Router::new()
                    .route(WORKER_DOMAIN_PATH, get({
                        let domain_calls = domain_calls.clone();
                        let original = original.clone();
                        let expected = identity.clone();
                        move |headers: HeaderMap, Query(actual): Query<awaken_worker_registry::WorkerIdentity>| {
                            let domain_calls = domain_calls.clone();
                            let original = original.clone();
                            let expected = expected.clone();
                            async move {
                                domain_calls.fetch_add(1, Ordering::SeqCst);
                                assert_eq!(actual, expected, "R10 exact encoded query");
                                assert_eq!(headers.get(header::AUTHORIZATION).unwrap(), "Bearer secret");
                                if rule == "bad-json" {
                                    return (StatusCode::OK, "{").into_response();
                                }
                                let observation = WorkerDomainObservation {
                                    registered: original,
                                    physical_attempt_active: true,
                                    work_claims_active: true,
                                    session_realizations_active: true,
                                    retirement_permitted: None,
                                };
                                let mut value = serde_json::to_value(observation).unwrap();
                                let keys = value.as_object().unwrap().keys().map(String::as_str).collect::<std::collections::BTreeSet<_>>();
                                assert_eq!(keys, std::collections::BTreeSet::from(["registered", "physical_attempt_active", "work_claims_active", "session_realizations_active"]), "R6 original four-key wire");
                                let status = match rule {
                                    "identity" => {
                                        value["registered"]["snapshot"]["identity"]["generation"] = serde_json::json!(3);
                                        StatusCode::OK
                                    }
                                    "field" => {
                                        value.as_object_mut().unwrap().remove("physical_attempt_active");
                                        StatusCode::OK
                                    }
                                    "accepted" => StatusCode::ACCEPTED,
                                    "no-content" => StatusCode::NO_CONTENT,
                                    "missing" => StatusCode::CONFLICT,
                                    "bad-query" => StatusCode::BAD_REQUEST,
                                    "unauthorized" => StatusCode::UNAUTHORIZED,
                                    "unavailable" => StatusCode::SERVICE_UNAVAILABLE,
                                    _ => StatusCode::OK,
                                };
                                (status, Json(value)).into_response()
                            }
                        }
                    }))
                    .route(WORKER_OBSERVATIONS_PATH, get({
                        let list_calls = list_calls.clone();
                        move || {
                            let list_calls = list_calls.clone();
                            async move {
                                list_calls.fetch_add(1, Ordering::SeqCst);
                                StatusCode::CONFLICT
                            }
                        }
                    }));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let source =
                HttpWorkerObservationSource::new(format!("http://{address}"), "secret").unwrap();
            let point_source: &dyn WorkerIncarnationSource = &source;
            let result = point_source.incarnation(&identity).await;
            match rule {
                "exact" => assert_eq!(result.unwrap(), Some(original.clone()), "R6"),
                "missing" => assert_eq!(result.unwrap(), None, "R8"),
                _ => assert!(
                    matches!(result, Err(RegistryError::Persistence(_))),
                    "R7/R8/R9 {rule}"
                ),
            }
            assert_eq!(
                domain_calls.load(Ordering::SeqCst),
                if rule == "unavailable" { ATTEMPTS } else { 1 },
                "R9 {rule}"
            );
            assert_eq!(
                list_calls.load(Ordering::SeqCst),
                0,
                "no historical fallback"
            );
            assert_eq!(
                domain_calls.load(Ordering::SeqCst),
                if rule == "unavailable" { ATTEMPTS } else { 1 }
            );
            assert!(source.list().await.is_err(), "R8 list409 remains a refusal");
            assert_eq!(list_calls.load(Ordering::SeqCst), 1);
            server.abort();
            let _ = server.await;
        }
    }

    struct RotatingTokenSource(std::sync::atomic::AtomicUsize);

    impl awaken_service_auth_contract::ServiceBearerTokenSource for RotatingTokenSource {
        fn current_token(&self) -> Result<Arc<str>, String> {
            // Constructor and first request see the original token. The
            // existing unauthorized boundary then observes one real rotation.
            let read = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Arc::from(if read < 2 { "before" } else { "after" }))
        }
    }

    #[tokio::test]
    async fn historical_http_read_reuses_token_rotation_and_transient_retry_policy() {
        // R9 changed token after401 -> one authenticated retry; unchanged
        // token -> immediate refusal (the table above). Two503 responses then
        // exact200 use exactly the original three attempts, not another loop.
        // Connection refusal remains unavailable, never missing history.
        use std::sync::atomic::{AtomicUsize, Ordering};
        let identity = awaken_worker_registry::WorkerIdentity::new("historical", "original", 1);
        let original = historical_worker(identity.clone());
        for rotating in [false, true] {
            let calls = Arc::new(AtomicUsize::new(0));
            let app = Router::new().route(
                WORKER_DOMAIN_PATH,
                get({
                    let calls = calls.clone();
                    let original = original.clone();
                    move |headers: HeaderMap| {
                        let calls = calls.clone();
                        let original = original.clone();
                        async move {
                            let number = calls.fetch_add(1, Ordering::SeqCst);
                            let status = if rotating
                                && headers.get(header::AUTHORIZATION).unwrap() == "Bearer before"
                            {
                                StatusCode::UNAUTHORIZED
                            } else if !rotating && number < 2 {
                                StatusCode::SERVICE_UNAVAILABLE
                            } else {
                                StatusCode::OK
                            };
                            (
                                status,
                                Json(WorkerDomainObservation {
                                    registered: original,
                                    physical_attempt_active: false,
                                    work_claims_active: false,
                                    session_realizations_active: false,
                                    retirement_permitted: None,
                                }),
                            )
                                .into_response()
                        }
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let tokens: Arc<dyn awaken_service_auth_contract::ServiceBearerTokenSource> =
                if rotating {
                    Arc::new(RotatingTokenSource(AtomicUsize::new(0)))
                } else {
                    awaken_service_auth_contract::static_token_source("secret").unwrap()
                };
            let source =
                HttpWorkerObservationSource::with_token_source(format!("http://{address}"), tokens)
                    .unwrap();
            assert_eq!(
                source.incarnation(&identity).await.unwrap(),
                Some(original.clone()),
                "R9"
            );
            assert_eq!(
                calls.load(Ordering::SeqCst),
                if rotating { 2 } else { ATTEMPTS }
            );
            server.abort();
            let _ = server.await;
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let source =
            HttpWorkerObservationSource::new(format!("http://{address}"), "secret").unwrap();
        assert!(
            matches!(
                source.incarnation(&identity).await,
                Err(RegistryError::Persistence(_))
            ),
            "R9 connection refusal is unavailable, not None"
        );
    }

    #[tokio::test]
    async fn boundary_and_remote_adapter_preserve_auth_and_read_only_projection() {
        // Cause/effect graph: private token + Coordinator source -> authenticated
        // GET -> serialized secret-free observations -> Control read port.
        // Decision table: R1 valid token + healthy source -> exact projection;
        // R2 missing/wrong token -> 401 before source access; R3 malformed URL or
        // empty token -> adapter construction fails; R4 observation source
        // failure -> 503 and bounded client retry; R5 authenticator dependency
        // failure -> 503 (not a false credential rejection). This test owns
        // R1-R3/R5; R4 uses the same generic service-error branch covered by the
        // Control service boundary suite.
        let app = router(Arc::new(FixedSource), "secret").expect("observation router");
        let unauthorized = app
            .clone()
            .oneshot(
                Request::get(WORKER_OBSERVATIONS_PATH)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED, "R2");

        let auth_unavailable =
            router_with_authenticator(Arc::new(FixedSource), Arc::new(UnavailableAuthenticator))
                .oneshot(
                    Request::get(WORKER_OBSERVATIONS_PATH)
                        .header("authorization", "Bearer secret")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
        assert_eq!(
            auth_unavailable.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "R5"
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let source = HttpWorkerObservationSource::new(format!("http://{address}"), "secret")
            .expect("R1 client");
        assert!(source.list().await.expect("R1 projection").is_empty());

        assert!(
            HttpWorkerObservationSource::new("ftp://coordinator", "secret").is_err(),
            "R3"
        );
        assert!(
            HttpWorkerObservationSource::new("http://coordinator", "").is_err(),
            "R3"
        );
    }
}
