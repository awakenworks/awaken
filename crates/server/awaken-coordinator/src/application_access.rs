//! Coordinator API for issuing narrow, short-lived application credentials.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use awaken_authz_enforce::{ApplicationGrant, ApplicationThreadBinding};
use awaken_session_contract::ManagedSessionRepository;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, post};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};

use crate::application_access_store::{
    ApplicationAccessRepositoryError, ApplicationAccessStore, now_unix_millis,
};
const DEFAULT_TTL_SECONDS: u64 = 300;

/// Canonical maximum for both legacy-v1 process-local and v2 durable
/// ApplicationAccess credentials. This value must not decrease until the
/// one-time v1-to-v2 retained-release drain and cutover has completed; Cloud
/// consumes the exact schema-v2 runtime profile projection as its drain bound.
pub const APPLICATION_ACCESS_MAX_TTL_SECONDS: u64 = 900;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CreateApplicationToken {
    pub protocols: Vec<String>,
    pub operations: Vec<String>,
    pub thread_bindings: Vec<CreateApplicationThreadBinding>,
    #[serde(default = "default_ttl")]
    pub expires_in_seconds: u64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CreateApplicationThreadBinding {
    pub external_thread_id: String,
    pub managed_session_id: String,
}

#[derive(Deserialize, Serialize)]
pub struct IssuedApplicationToken {
    pub id: String,
    pub object: String,
    pub token_type: String,
    pub access_token: String,
    pub expires_at: String,
    pub protocols: Vec<String>,
    pub operations: Vec<String>,
}

/// Typed failure from the one application-capability issuance use case.
///
/// Product relation owners may authorize a narrower relation before calling
/// [`issue_application_access`], but Session validation and credential minting
/// remain here rather than being reimplemented by each HTTP surface.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApplicationAccessIssuanceError {
    #[error("invalid application access request: {0}")]
    Invalid(&'static str),
    #[error("bound Managed Session not found")]
    SessionNotFound,
    #[error("Managed Session repository unavailable")]
    SessionUnavailable,
    #[error("application access conflict: {0}")]
    Conflict(&'static str),
    #[error("application access authority unavailable")]
    AuthorityUnavailable,
}

#[derive(Clone)]
struct ApplicationTokenState {
    store: Arc<ApplicationAccessStore>,
    sessions: Arc<dyn ManagedSessionRepository>,
    default_workspace: String,
}

pub fn router(
    store: Arc<ApplicationAccessStore>,
    sessions: Arc<dyn ManagedSessionRepository>,
    default_workspace: String,
) -> Router {
    Router::new()
        .route("/v1/application-access-tokens", post(create))
        .route("/v1/application-access-tokens/{id}", delete(revoke))
        .with_state(ApplicationTokenState {
            store,
            sessions,
            default_workspace,
        })
}

async fn create(
    State(state): State<ApplicationTokenState>,
    workspace: Option<Extension<awaken_tenancy::WorkspaceScope>>,
    Json(request): Json<CreateApplicationToken>,
) -> Response {
    let workspace_id = workspace
        .map(|Extension(scope)| scope.0)
        .unwrap_or(state.default_workspace);
    match issue_application_access(
        state.store.as_ref(),
        state.sessions.as_ref(),
        &workspace_id,
        request,
    )
    .await
    {
        Ok(issued) => {
            let mut response = (StatusCode::CREATED, Json(issued)).into_response();
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            response
        }
        Err(error) => issuance_problem(error),
    }
}

/// Issue one application capability after the calling product surface has
/// authorized its own relation.
///
/// This is the sole transport-neutral issuance owner. It validates the
/// canonical request vocabulary, resolves every bound Managed Session from the
/// authoritative Session repository, and mints through the durable
/// [`ApplicationAccessStore`]. HTTP routes add response headers and map the
/// typed result; they do not repeat this policy.
pub async fn issue_application_access(
    store: &ApplicationAccessStore,
    sessions: &dyn ManagedSessionRepository,
    workspace_id: &str,
    request: CreateApplicationToken,
) -> Result<IssuedApplicationToken, ApplicationAccessIssuanceError> {
    validate(&request).map_err(ApplicationAccessIssuanceError::Invalid)?;
    let permits_run = request
        .operations
        .iter()
        .any(|operation| operation == "thread.run");
    let mut thread_bindings = HashMap::new();
    for requested in &request.thread_bindings {
        let owner = match sessions.owner(&requested.managed_session_id).await {
            Ok(owner) => owner,
            Err(awaken_session_contract::SessionRepositoryError::NotFound) => {
                return Err(ApplicationAccessIssuanceError::SessionNotFound);
            }
            Err(_) => {
                return Err(ApplicationAccessIssuanceError::SessionUnavailable);
            }
        };
        if owner != workspace_id {
            return Err(ApplicationAccessIssuanceError::SessionNotFound);
        }
        let session = match sessions.get(&requested.managed_session_id).await {
            Ok(session) => session,
            Err(awaken_session_contract::SessionRepositoryError::NotFound) => {
                return Err(ApplicationAccessIssuanceError::SessionNotFound);
            }
            Err(_) => {
                return Err(ApplicationAccessIssuanceError::SessionUnavailable);
            }
        };
        let Some(agent_id) = session.agent_id() else {
            return Err(ApplicationAccessIssuanceError::Conflict(
                "bound Managed Session baseline is not frozen",
            ));
        };
        if permits_run && session.is_terminal() {
            return Err(ApplicationAccessIssuanceError::Conflict(
                "bound Managed Session is not runnable",
            ));
        }
        thread_bindings.insert(
            requested.external_thread_id.clone(),
            ApplicationThreadBinding {
                managed_session_id: requested.managed_session_id.clone(),
                agent_id: agent_id.to_string(),
            },
        );
    }
    let now_ms = now_unix_millis();
    let expires_ms = now_ms.saturating_add(request.expires_in_seconds.saturating_mul(1000));
    let expires_at = awaken_session_contract::epoch_millis_to_rfc3339(expires_ms);
    let grant = ApplicationGrant {
        protocols: request.protocols.iter().cloned().collect::<HashSet<_>>(),
        operations: request.operations.iter().cloned().collect::<HashSet<_>>(),
        thread_bindings,
    };
    let issued = store
        .mint(workspace_id.to_owned(), now_ms, expires_ms, grant)
        .await
        .map_err(|error| match error {
            ApplicationAccessRepositoryError::Invalid(_)
            | ApplicationAccessRepositoryError::NotFound
            | ApplicationAccessRepositoryError::Unavailable(_)
            | ApplicationAccessRepositoryError::Corrupt(_) => {
                ApplicationAccessIssuanceError::AuthorityUnavailable
            }
        })?;
    Ok(IssuedApplicationToken {
        id: issued.id,
        object: "application_access_token".into(),
        token_type: "Bearer".into(),
        access_token: issued.access_token,
        expires_at,
        protocols: request.protocols,
        operations: request.operations,
    })
}

fn issuance_problem(error: ApplicationAccessIssuanceError) -> Response {
    match error {
        ApplicationAccessIssuanceError::Invalid(detail) => {
            problem(StatusCode::UNPROCESSABLE_ENTITY, detail)
        }
        ApplicationAccessIssuanceError::SessionNotFound => {
            problem(StatusCode::NOT_FOUND, "bound Managed Session not found")
        }
        ApplicationAccessIssuanceError::SessionUnavailable => problem(
            StatusCode::SERVICE_UNAVAILABLE,
            "Managed Session repository unavailable",
        ),
        ApplicationAccessIssuanceError::Conflict(detail) => problem(StatusCode::CONFLICT, detail),
        ApplicationAccessIssuanceError::AuthorityUnavailable => problem(
            StatusCode::SERVICE_UNAVAILABLE,
            "application access authority unavailable",
        ),
    }
}

async fn revoke(
    State(state): State<ApplicationTokenState>,
    workspace: Option<Extension<awaken_tenancy::WorkspaceScope>>,
    Path(id): Path<String>,
) -> Response {
    let workspace_id = workspace
        .map(|Extension(scope)| scope.0)
        .unwrap_or(state.default_workspace);
    match state
        .store
        .revoke(&workspace_id, &id, now_unix_millis())
        .await
    {
        Ok(()) | Err(ApplicationAccessRepositoryError::NotFound) => {
            StatusCode::NO_CONTENT.into_response()
        }
        Err(ApplicationAccessRepositoryError::Invalid(_)) => problem(
            StatusCode::UNPROCESSABLE_ENTITY,
            "application token id must be an aat_-prefixed UUIDv7",
        ),
        Err(
            ApplicationAccessRepositoryError::Unavailable(_)
            | ApplicationAccessRepositoryError::Corrupt(_),
        ) => problem(
            StatusCode::SERVICE_UNAVAILABLE,
            "application access authority unavailable",
        ),
    }
}

fn validate(request: &CreateApplicationToken) -> Result<(), &'static str> {
    if request.expires_in_seconds == 0
        || request.expires_in_seconds > APPLICATION_ACCESS_MAX_TTL_SECONDS
    {
        return Err("expires_in_seconds is outside the supported application access TTL");
    }
    let protocols = request.protocols.iter().cloned().collect::<HashSet<_>>();
    if protocols.len() != request.protocols.len() {
        return Err("protocols must be unique");
    }
    let operations = request.operations.iter().cloned().collect::<HashSet<_>>();
    if operations.len() != request.operations.len() {
        return Err("operations must be unique");
    }
    let thread_bindings = request
        .thread_bindings
        .iter()
        .map(|binding| {
            (
                binding.external_thread_id.clone(),
                ApplicationThreadBinding {
                    managed_session_id: binding.managed_session_id.clone(),
                    // Admission has not read the frozen Session yet. The actual
                    // Agent is filled from that authority before mint; this
                    // non-empty sentinel lets the grant owner validate every
                    // request coordinate before those bounded repository reads.
                    agent_id: "pending-session-authority".into(),
                },
            )
        })
        .collect::<HashMap<_, _>>();
    if thread_bindings.len() != request.thread_bindings.len() {
        return Err("thread_bindings must be one-to-one and unique");
    }
    ApplicationGrant {
        protocols,
        operations,
        thread_bindings,
    }
    .validate()
}

fn problem(status: StatusCode, detail: impl Into<String>) -> Response {
    (
        status,
        Json(serde_json::json!({
            "type": "about:blank",
            "title": status.canonical_reason().unwrap_or("Application access error"),
            "status": status.as_u16(),
            "detail": detail.into(),
        })),
    )
        .into_response()
}

const fn default_ttl() -> u64 {
    DEFAULT_TTL_SECONDS
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use async_trait::async_trait;
    use awaken_authz_enforce::ApplicationAccessAuthenticator;
    use awaken_session_contract::{
        EnvironmentFingerprint, EnvironmentSnapshot, ManagedLifecycleFact, PersistedSession,
        SessionBaseline, SessionBaselineInputs, SessionBaselineState, SessionMcpAuthoringContext,
        SessionNetworkPolicy, SessionRuntimePlacement,
    };
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use super::*;

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum TestSessionReadFailure {
        Owner,
        Get,
    }

    struct TestSessions {
        rows: HashMap<String, (String, PersistedSession)>,
        failure: Option<TestSessionReadFailure>,
    }

    #[async_trait]
    impl ManagedSessionRepository for TestSessions {
        async fn create(
            &self,
            _owner_scope: &str,
            _session: PersistedSession,
            _idempotency: awaken_session_contract::IdempotencyRecord,
            _lifecycle_facts: Vec<ManagedLifecycleFact>,
        ) -> Result<
            awaken_session_contract::SessionCreateResult,
            awaken_session_contract::SessionRepositoryError,
        > {
            Err(
                awaken_session_contract::SessionRepositoryError::Unavailable(
                    "read-only TestSessions".into(),
                ),
            )
        }

        async fn replay_create(
            &self,
            _owner_scope: &str,
            _session_id: &str,
            _idempotency: &awaken_session_contract::IdempotencyRecord,
        ) -> Result<Option<PersistedSession>, awaken_session_contract::SessionRepositoryError>
        {
            Err(
                awaken_session_contract::SessionRepositoryError::Unavailable(
                    "read-only TestSessions".into(),
                ),
            )
        }

        async fn commit_mutation(
            &self,
            _owner_scope: &str,
            _mutation: awaken_session_contract::SessionMutation,
        ) -> Result<
            awaken_session_contract::SessionMutationResult,
            awaken_session_contract::SessionRepositoryError,
        > {
            Err(
                awaken_session_contract::SessionRepositoryError::Unavailable(
                    "read-only TestSessions".into(),
                ),
            )
        }

        async fn append_lifecycle(
            &self,
            _fact: ManagedLifecycleFact,
        ) -> Result<(), awaken_session_contract::SessionRepositoryError> {
            Ok(())
        }

        async fn pending_lifecycle(
            &self,
        ) -> Result<Vec<ManagedLifecycleFact>, awaken_session_contract::SessionRepositoryError>
        {
            Ok(Vec::new())
        }

        async fn complete_lifecycle(
            &self,
            _fact_id: &str,
        ) -> Result<(), awaken_session_contract::SessionRepositoryError> {
            Ok(())
        }

        async fn get(
            &self,
            session_id: &str,
        ) -> Result<PersistedSession, awaken_session_contract::SessionRepositoryError> {
            if self.failure == Some(TestSessionReadFailure::Get) {
                return Err(awaken_session_contract::SessionRepositoryError::Corrupt(
                    "get-backend-diagnostic-456".into(),
                ));
            }
            self.rows
                .get(session_id)
                .map(|(_, session)| session.clone())
                .ok_or(awaken_session_contract::SessionRepositoryError::NotFound)
        }

        async fn reconcilable_sessions(
            &self,
        ) -> Result<
            awaken_session_contract::SessionRecoveryScan,
            awaken_session_contract::SessionRepositoryError,
        > {
            Ok(awaken_session_contract::SessionRecoveryScan::default())
        }

        async fn sessions_referencing_credential_source(
            &self,
            _workspace_id: &str,
            _source_id: &awaken_credential_contract::CredentialSourceId,
        ) -> Result<Vec<PersistedSession>, awaken_session_contract::SessionRepositoryError>
        {
            Err(
                awaken_session_contract::SessionRepositoryError::Unavailable(
                    "read-only TestSessions does not own credential dependency indexing".into(),
                ),
            )
        }

        async fn idempotency_receipt(
            &self,
            _session_id: &str,
            _key: &str,
        ) -> Result<
            Option<awaken_session_contract::SessionIdempotencyReceipt>,
            awaken_session_contract::SessionRepositoryError,
        > {
            Ok(None)
        }

        async fn owner(
            &self,
            session_id: &str,
        ) -> Result<String, awaken_session_contract::SessionRepositoryError> {
            if self.failure == Some(TestSessionReadFailure::Owner) {
                return Err(
                    awaken_session_contract::SessionRepositoryError::Unavailable(
                        "owner-backend-diagnostic-123".into(),
                    ),
                );
            }
            self.rows
                .get(session_id)
                .map(|(owner, _)| owner.clone())
                .ok_or(awaken_session_contract::SessionRepositoryError::NotFound)
        }
    }

    fn frozen_session(id: &str, status: &str) -> PersistedSession {
        // Test fixtures use the same closed constructors as production. A JSON
        // object here previously drifted behind the required Environment facts
        // and caused authorization tests to fail before exercising policy.
        let baseline =
            SessionBaselineState::Frozen(SessionBaseline::compile(SessionBaselineInputs {
                environment: EnvironmentSnapshot {
                    environment_id: "env".into(),
                    revision: awaken_environment_contract::EnvironmentRevision(1),
                    self_hosted: false,
                    config_fingerprint: EnvironmentFingerprint("env-1".into()),
                    sandbox: Default::default(),
                    sandbox_provisioning: Default::default(),
                    idle_retention: Default::default(),
                    packages: Default::default(),
                    prepared_image: None,
                    network: SessionNetworkPolicy::None,
                    credential_realization:
                        awaken_credential_contract::CredentialRealizationProfile::self_hosted_acp(),
                },
                runtime_placement: SessionRuntimePlacement::Local,
                mcp_authoring: SessionMcpAuthoringContext::default(),
                agent_id: "support".into(),
                agent_revision: None,
                model: "test-model".into(),
                model_override: None,
                runtime: None,
                delegate_ids: Vec::new(),
                toolsets: Vec::new(),
                mounts: Vec::new(),
                env: Vec::new(),
                prompts: Vec::new(),
                transcript_prefix: None,
            }));
        PersistedSession {
            session_id: id.into(),
            revision: Default::default(),
            baseline,
            title: None,
            metadata: BTreeMap::new(),
            tools: Default::default(),
            event_batches: Vec::new(),
            activity_epoch: 0,
            active_activity_epochs: Default::default(),
            running_interval: None,
            closed_runtime_intervals: Vec::new(),
            runtime_active_millis: 0,
            usage_cursor: Default::default(),
            budget: Default::default(),
            environment: Default::default(),
            mcp: Default::default(),
            resources: Default::default(),
            realization: None,
            realization_progress: Default::default(),
            execution: status.parse().expect("fixture execution state"),
            disposition: Default::default(),
            terminal_cleanup: Default::default(),
        }
    }

    fn preparing_session(id: &str) -> PersistedSession {
        let mut session = frozen_session(id, "preparing");
        let SessionBaselineState::Frozen(baseline) = session.baseline else {
            unreachable!()
        };
        session.baseline =
            SessionBaselineState::Preparing(awaken_session_contract::SessionCreationIntent {
                control: awaken_session_contract::ControlSessionCreationInputs {
                    mutation_policy: awaken_session_contract::SessionMutationPolicy::Managed,
                    environment: baseline.environment,
                    runtime_placement: awaken_session_contract::SessionRuntimePlacement::Local,
                    agent_id: baseline.agent_id,
                    agent_revision: baseline.agent_revision,
                    model_override: baseline.model_override,
                    system_prompt: *baseline.system_prompt,
                    model: baseline.model.clone(),
                    execution_model_ref: baseline.model,
                    runtime: baseline.runtime,
                    mcp_authoring: Default::default(),
                    toolsets: baseline.toolsets,
                    delegate_ids: baseline.delegate_ids,
                    mounts: baseline.mounts,
                    env: baseline.env,
                    prompts: baseline.prompts,
                    transcript_prefix: baseline.transcript_prefix,
                    resources: Default::default(),
                    initial_mcp: Vec::new(),
                },
            });
        session
    }

    fn test_sessions(
        rows: Vec<(&str, &str, PersistedSession)>,
        failure: Option<TestSessionReadFailure>,
    ) -> Arc<TestSessions> {
        Arc::new(TestSessions {
            rows: rows
                .into_iter()
                .map(|(id, owner, session)| (id.to_string(), (owner.to_string(), session)))
                .collect(),
            failure,
        })
    }

    fn sessions(rows: Vec<(&str, &str, PersistedSession)>) -> Arc<TestSessions> {
        test_sessions(rows, None)
    }

    fn application_access() -> Arc<ApplicationAccessStore> {
        Arc::new(ApplicationAccessStore::open_in_memory().unwrap())
    }

    /// Cause-effect serialization rules: R1 a complete canonical issuance
    /// request (C1) serializes every authorization input and deserializes back
    /// into that same Open-owned request shape (E1); R2 a complete canonical
    /// issued credential (C2) deserializes as an owned response after its input
    /// buffer is released (E2). Together they prevent clients from needing a
    /// parallel JSON DTO while keeping token issuance and validation here.
    #[test]
    fn canonical_application_access_wire_round_trips() {
        let request = CreateApplicationToken {
            protocols: vec!["ai-sdk".into()],
            operations: vec!["thread.run".into(), "thread.messages.read".into()],
            thread_bindings: vec![CreateApplicationThreadBinding {
                external_thread_id: "customer-thread".into(),
                managed_session_id: "sesn_1".into(),
            }],
            expires_in_seconds: APPLICATION_ACCESS_MAX_TTL_SECONDS,
        };
        let request_wire = serde_json::to_vec(&request).unwrap();
        let decoded_request: CreateApplicationToken =
            serde_json::from_slice(&request_wire).unwrap();
        assert_eq!(
            decoded_request.thread_bindings[0].managed_session_id,
            request.thread_bindings[0].managed_session_id,
            "R1"
        );
        assert_eq!(
            decoded_request.expires_in_seconds, APPLICATION_ACCESS_MAX_TTL_SECONDS,
            "R1"
        );

        let issued = IssuedApplicationToken {
            id: "aat_1".into(),
            object: "application_access_token".into(),
            token_type: "Bearer".into(),
            access_token: "opaque-application-token".into(), // awaken-allow: secret
            expires_at: "2026-08-18T12:00:00Z".into(),
            protocols: vec!["ai-sdk".into()],
            operations: vec!["thread.run".into(), "thread.messages.read".into()],
        };
        let issued_wire = serde_json::to_vec(&issued).unwrap();
        let decoded_issued = decode_owned::<IssuedApplicationToken>(&issued_wire);
        drop(issued_wire);
        assert_eq!(decoded_issued.object, "application_access_token", "R2");
        assert_eq!(decoded_issued.token_type, "Bearer", "R2");
        assert_eq!(decoded_issued.access_token, issued.access_token, "R2");
        assert_eq!(decoded_issued.expires_at, issued.expires_at, "R2");
    }

    fn decode_owned<T: serde::de::DeserializeOwned>(wire: &[u8]) -> T {
        serde_json::from_slice(wire).unwrap()
    }

    /// Transport-neutral issuance cause/effect table:
    ///
    /// | Rule | product relation | Session owner/frozen/live | Effect |
    /// |---|---|---|---|
    /// | U1 | already authorized | exact/yes/yes | one durable credential with exact binding |
    /// | U2 | already authorized | foreign/any/any | typed absence and no credential |
    ///
    /// HTTP header/status projection is covered by the route tests below. This
    /// table proves non-HTTP product surfaces reuse the same Session validation
    /// and durable issuer rather than calling the repository or mint primitive.
    #[tokio::test]
    async fn transport_neutral_issuance_owns_session_validation_and_minting() {
        let store = application_access();
        let repository = sessions(vec![(
            "sesn_1",
            "workspace-a",
            frozen_session("sesn_1", "idle"),
        )]);
        let request = || CreateApplicationToken {
            protocols: vec!["ai-sdk".into()],
            operations: vec!["thread.run".into(), "thread.messages.read".into()],
            thread_bindings: vec![CreateApplicationThreadBinding {
                external_thread_id: "customer-thread".into(),
                managed_session_id: "sesn_1".into(),
            }],
            expires_in_seconds: DEFAULT_TTL_SECONDS,
        };

        let issued = issue_application_access(
            store.as_ref(),
            repository.as_ref(),
            "workspace-a",
            request(),
        )
        .await
        .expect("U1 canonical issuance");
        let identity = store.authenticate(&issued.access_token).await.unwrap();
        assert_eq!(identity.workspace_id, "workspace-a", "U1");
        assert_eq!(
            identity.grant.thread_bindings["customer-thread"].managed_session_id, "sesn_1",
            "U1"
        );

        let foreign = issue_application_access(
            store.as_ref(),
            repository.as_ref(),
            "workspace-b",
            request(),
        )
        .await;
        assert!(
            matches!(
                foreign,
                Err(ApplicationAccessIssuanceError::SessionNotFound)
            ),
            "U2"
        );
    }

    /// TTL admission cause/effect table:
    ///
    /// | Rule | requested seconds | Effect |
    /// |---|---:|---|
    /// | T0 | legacy-v1 ceiling | canonical maximum remains 900 until retained cutover completes |
    /// | T1 | 1..=canonical maximum | admitted |
    /// | T2 | 0 | rejected before persistence |
    /// | T3 | canonical maximum + 1 | rejected before persistence |
    ///
    /// The exported maximum is the single contract used by remote product
    /// clients when sizing a credential that can still start a Run.
    #[test]
    fn application_access_ttl_uses_the_canonical_maximum() {
        // This test literal is compatibility evidence, not a second runtime
        // authority. Remove it only together with the one-time legacy-v1 drain
        // contract after every retained environment has completed that cutover.
        assert_eq!(APPLICATION_ACCESS_MAX_TTL_SECONDS, 900, "T0");
        let request = |expires_in_seconds| CreateApplicationToken {
            protocols: vec!["ai-sdk".into()],
            operations: vec!["thread.run".into()],
            thread_bindings: vec![CreateApplicationThreadBinding {
                external_thread_id: "customer-thread".into(),
                managed_session_id: "sesn_1".into(),
            }],
            expires_in_seconds,
        };

        assert_eq!(
            validate(&request(APPLICATION_ACCESS_MAX_TTL_SECONDS)),
            Ok(()),
            "T1"
        );
        assert_eq!(
            validate(&request(0)),
            Err("expires_in_seconds is outside the supported application access TTL"),
            "T2"
        );
        assert_eq!(
            validate(&request(
                APPLICATION_ACCESS_MAX_TTL_SECONDS.saturating_add(1)
            )),
            Err("expires_in_seconds is outside the supported application access TTL"),
            "T3"
        );
    }

    fn issue_body(session_id: &str, operations: Value) -> Value {
        json!({
            "protocols": ["ai-sdk"],
            "operations": operations,
            "thread_bindings": [{
                "external_thread_id": "customer-thread",
                "managed_session_id": session_id
            }]
        })
    }

    async fn issue(
        store: Arc<ApplicationAccessStore>,
        sessions: Arc<dyn ManagedSessionRepository>,
        body: Value,
    ) -> axum::response::Response {
        router(store, sessions, "workspace-a".to_string())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/application-access-tokens")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    /// Cause-effect graph: syntactically valid capability (C1), Session exists
    /// in caller Workspace (C2), frozen baseline (C3), and runnable lifecycle
    /// when run is requested (C4) produce a token carrying the exact Session and
    /// baseline Agent (E1). Rule T1: C1-C4 true -> 201/E1.
    #[tokio::test]
    async fn issues_an_exact_binding_from_the_managed_session_authority() {
        let store = application_access();
        let response = issue(
            store.clone(),
            sessions(vec![(
                "sesn_1",
                "workspace-a",
                frozen_session("sesn_1", "idle"),
            )]),
            issue_body("sesn_1", json!(["thread.run", "thread.messages.read"])),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store",
            "T1 cleartext mint response is never cacheable"
        );
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        let token = body["access_token"].as_str().unwrap();
        let identity = store.authenticate(token).await.unwrap();
        assert_eq!(identity.workspace_id, "workspace-a");
        assert_eq!(identity.grant.protocols, HashSet::from(["ai-sdk".into()]));
        assert!(identity.grant.operations.contains("thread.run"));
        assert!(identity.grant.operations.contains("thread.messages.read"));
        assert_eq!(
            identity.grant.thread_bindings["customer-thread"],
            ApplicationThreadBinding {
                managed_session_id: "sesn_1".into(),
                agent_id: "support".into(),
            }
        );
    }

    /// Decision rules: T2 missing or foreign Session (C2 false) -> 404 with no
    /// token; the same response prevents cross-Workspace existence disclosure.
    #[tokio::test]
    async fn hides_missing_and_foreign_sessions() {
        let repository = sessions(vec![(
            "sesn_foreign",
            "workspace-b",
            frozen_session("sesn_foreign", "idle"),
        )]);
        for id in ["sesn_missing", "sesn_foreign"] {
            let response = issue(
                application_access(),
                repository.clone(),
                issue_body(id, json!(["thread.run"])),
            )
            .await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "case {id}");
        }
    }

    /// Decision rules: T3 unfrozen baseline (C3 false) -> 409; T4 terminal plus
    /// run requested (C4 false) -> 409; T5 terminal plus history-only -> 201,
    /// because reads do not create a new runtime transition.
    #[tokio::test]
    async fn lifecycle_and_operation_determine_issuance() {
        let repository = sessions(vec![
            (
                "sesn_preparing",
                "workspace-a",
                preparing_session("sesn_preparing"),
            ),
            (
                "sesn_terminal",
                "workspace-a",
                frozen_session("sesn_terminal", "terminated"),
            ),
        ]);
        let preparing = issue(
            application_access(),
            repository.clone(),
            issue_body("sesn_preparing", json!(["thread.run"])),
        )
        .await;
        assert_eq!(preparing.status(), StatusCode::CONFLICT);

        let terminal_run = issue(
            application_access(),
            repository.clone(),
            issue_body("sesn_terminal", json!(["thread.run"])),
        )
        .await;
        assert_eq!(terminal_run.status(), StatusCode::CONFLICT);

        let terminal_read = issue(
            application_access(),
            repository,
            issue_body("sesn_terminal", json!(["thread.messages.read"])),
        )
        .await;
        assert_eq!(terminal_read.status(), StatusCode::CREATED);
    }

    /// T6 invalid capability vocabulary or non-bijective bindings (C1 false)
    /// -> 422. These cases ensure no compatibility aliases or overlapping
    /// external/session identities become a second authorization source.
    #[tokio::test]
    async fn invalid_or_duplicate_capabilities_are_rejected() {
        let repository = sessions(vec![(
            "sesn_1",
            "workspace-a",
            frozen_session("sesn_1", "idle"),
        )]);
        let cases = [
            json!({
                "protocols": ["unknown"],
                "operations": ["thread.run"],
                "thread_bindings": [{
                    "external_thread_id": "customer-thread",
                    "managed_session_id": "sesn_1"
                }]
            }),
            json!({
                "protocols": ["ai-sdk"],
                "operations": ["thread.read"],
                "thread_bindings": [{
                    "external_thread_id": "customer-thread",
                    "managed_session_id": "sesn_1"
                }]
            }),
            json!({
                "protocols": ["ai-sdk"],
                "operations": ["thread.run"],
                "thread_bindings": [
                    { "external_thread_id": "one", "managed_session_id": "sesn_1" },
                    { "external_thread_id": "two", "managed_session_id": "sesn_1" }
                ]
            }),
            json!({
                "protocols": ["ai-sdk", "ai-sdk"],
                "operations": ["thread.run"],
                "thread_bindings": [{
                    "external_thread_id": "customer-thread",
                    "managed_session_id": "sesn_1"
                }]
            }),
        ];
        for body in cases {
            let response = issue(application_access(), repository.clone(), body).await;
            assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        }
    }

    /// Request-admission cause/effect table:
    ///
    /// | Rule | binding count | each id bytes | unknown field | Effect |
    /// |---|---:|---:|---|---|
    /// | B1 | 1..=32 | 1..=255 | no | admitted to Session authority checks |
    /// | B2 | 0 or >32 | any | no | 422 before any Session lookup |
    /// | B3 | 1..=32 | 0 or >255 | no | 422 before any Session lookup |
    /// | B4 | any | any | request/binding | 422; no legacy field is ignored |
    ///
    /// The two authoritative Session reads per binding are therefore bounded at
    /// 64 per admitted mint; no unbounded request can amplify repository work.
    #[tokio::test]
    async fn request_shape_and_session_lookup_work_are_bounded() {
        // These literals are compatibility assertions against the canonical
        // ApplicationGrant validator, not a second production limit owner.
        const CONTRACT_MAX_BINDINGS: usize = 32;
        const CONTRACT_MAX_ID_BYTES: usize = 255;
        let binding = |index: usize, bytes: usize| CreateApplicationThreadBinding {
            external_thread_id: format!("{index}-{}", "x".repeat(bytes.saturating_sub(3))),
            managed_session_id: format!("{index}-{}", "s".repeat(bytes.saturating_sub(3))),
        };
        let request = |thread_bindings| CreateApplicationToken {
            protocols: vec!["ai-sdk".into()],
            operations: vec!["thread.run".into()],
            thread_bindings,
            expires_in_seconds: DEFAULT_TTL_SECONDS,
        };

        let maximum = request(
            (0..CONTRACT_MAX_BINDINGS)
                .map(|index| binding(index, CONTRACT_MAX_ID_BYTES))
                .collect(),
        );
        assert_eq!(validate(&maximum), Ok(()), "B1");
        assert_eq!(
            validate(&request(Vec::new())),
            Err("thread_bindings must contain between 1 and 32 bindings"),
            "B2 empty"
        );
        assert_eq!(
            validate(&request(
                (0..=CONTRACT_MAX_BINDINGS)
                    .map(|index| binding(index, 16))
                    .collect()
            )),
            Err("thread_bindings must contain between 1 and 32 bindings"),
            "B2 over maximum"
        );
        for overlong in [
            CreateApplicationThreadBinding {
                external_thread_id: "x".repeat(CONTRACT_MAX_ID_BYTES + 1),
                managed_session_id: "sesn_1".into(),
            },
            CreateApplicationThreadBinding {
                external_thread_id: "thread-1".into(),
                managed_session_id: "s".repeat(CONTRACT_MAX_ID_BYTES + 1),
            },
        ] {
            assert_eq!(
                validate(&request(vec![overlong])),
                Err("thread binding ids must contain between 1 and 255 bytes"),
                "B3"
            );
        }

        let repository = sessions(vec![(
            "sesn_1",
            "workspace-a",
            frozen_session("sesn_1", "idle"),
        )]);
        for field in ["authority_id", "application_scope", "actor_key"] {
            let mut body = issue_body("sesn_1", json!(["thread.run"]));
            body.as_object_mut()
                .unwrap()
                .insert(field.into(), json!("legacy-dead-field"));
            assert_eq!(
                issue(application_access(), repository.clone(), body)
                    .await
                    .status(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "B4 {field}"
            );
        }
        let mut body = issue_body("sesn_1", json!(["thread.run"]));
        body["thread_bindings"][0]["agent_id"] = json!("caller-invented-agent");
        assert_eq!(
            issue(application_access(), repository, body).await.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "B4 binding"
        );
    }

    /// Session-authority failure table: S1 owner read unavailable and S2
    /// aggregate read corrupt are distinct internal causes. Both produce the
    /// same fixed 503 (E1), expose no adapter detail (E2), and mint no durable
    /// credential (E3). Missing/foreign remain the separate 404 rules above.
    #[tokio::test]
    async fn session_repository_failures_have_one_non_disclosing_503() {
        let mut expected = None;
        for failure in [TestSessionReadFailure::Owner, TestSessionReadFailure::Get] {
            let repository = test_sessions(
                vec![("sesn_1", "workspace-a", frozen_session("sesn_1", "idle"))],
                Some(failure),
            );
            let response = issue(
                application_access(),
                repository,
                issue_body("sesn_1", json!(["thread.run"])),
            )
            .await;
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            assert_eq!(
                serde_json::from_slice::<Value>(&body).unwrap()["detail"],
                "Managed Session repository unavailable"
            );
            assert!(!String::from_utf8_lossy(&body).contains("diagnostic-"));
            if let Some(expected) = &expected {
                assert_eq!(&body, expected, "S1/S2 fixed public response");
            } else {
                expected = Some(body);
            }
        }
    }

    /// Revocation cause/effect rules: D1 a live id, D2 an unknown but valid
    /// UUIDv7 id, and D3 a foreign Workspace select the public absence effect.
    /// R1 exact/live or repeated -> 204 and revoked; R2 exact/unknown -> 204;
    /// R3 foreign/existing -> 204 but the credential remains live for its owner.
    /// No caller can revoke across the Workspace fence or must coordinate a
    /// read-before-delete/target the replica that minted the credential.
    #[tokio::test]
    async fn revoke_is_globally_idempotent() {
        let store = application_access();
        let repository = sessions(vec![(
            "sesn_1",
            "workspace-a",
            frozen_session("sesn_1", "idle"),
        )]);
        let response = issue(
            store.clone(),
            repository.clone(),
            issue_body("sesn_1", json!(["thread.run"])),
        )
        .await;
        let body: IssuedApplicationToken =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        let foreign = router(store.clone(), repository.clone(), "workspace-b".into());
        let response = foreign
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/v1/application-access-tokens/{}", body.id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT, "R3");
        assert!(store.authenticate(&body.access_token).await.is_ok(), "R3");

        let app = router(store.clone(), repository, "workspace-a".into());
        for id in [body.id, format!("aat_{}", uuid::Uuid::now_v7())] {
            for attempt in 0..2 {
                let response = app
                    .clone()
                    .oneshot(
                        Request::builder()
                            .method("DELETE")
                            .uri(format!("/v1/application-access-tokens/{id}"))
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::NO_CONTENT, "{id}/{attempt}");
            }
        }
        assert_eq!(
            store.authenticate(&body.access_token).await,
            Err(awaken_authz_enforce::ApplicationAuthenticationError::Invalid),
            "R1 revoked credential converges on the PEP's invalid/401 outcome"
        );
    }

    /// Repository failure disclosure cause/effect table: F1 unavailable and F2
    /// corrupt are distinct internal causes but both have the same public 503
    /// effect for mint and revoke; F3 malformed management id has the fixed,
    /// non-sensitive 422 contract. No backend URL, SQL, or row data crosses the
    /// management HTTP boundary.
    #[tokio::test]
    async fn repository_failures_have_one_non_disclosing_public_response() {
        let repository = sessions(vec![(
            "sesn_1",
            "workspace-a",
            frozen_session("sesn_1", "idle"),
        )]);
        let mut expected_create = None;
        let mut expected_revoke = None;
        for error in [
            ApplicationAccessRepositoryError::Unavailable(
                "backend-unavailable-diagnostic-123".into(),
            ),
            ApplicationAccessRepositoryError::Corrupt("corrupt-row-diagnostic-456".into()),
        ] {
            let store = Arc::new(ApplicationAccessStore::failing_for_test(error));
            let create = issue(
                store.clone(),
                repository.clone(),
                issue_body("sesn_1", json!(["thread.run"])),
            )
            .await;
            assert_eq!(create.status(), StatusCode::SERVICE_UNAVAILABLE);
            let create_body = to_bytes(create.into_body(), usize::MAX).await.unwrap();
            assert_eq!(
                serde_json::from_slice::<Value>(&create_body).unwrap()["detail"],
                "application access authority unavailable"
            );
            assert!(!String::from_utf8_lossy(&create_body).contains("diagnostic-"));
            if let Some(expected) = &expected_create {
                assert_eq!(&create_body, expected, "F1/F2 create");
            } else {
                expected_create = Some(create_body);
            }

            let revoke = router(store, repository.clone(), "workspace-a".into())
                .oneshot(
                    Request::builder()
                        .method("DELETE")
                        .uri(format!(
                            "/v1/application-access-tokens/aat_{}",
                            uuid::Uuid::now_v7()
                        ))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(revoke.status(), StatusCode::SERVICE_UNAVAILABLE);
            let revoke_body = to_bytes(revoke.into_body(), usize::MAX).await.unwrap();
            assert_eq!(
                serde_json::from_slice::<Value>(&revoke_body).unwrap()["detail"],
                "application access authority unavailable"
            );
            assert!(!String::from_utf8_lossy(&revoke_body).contains("diagnostic-"));
            if let Some(expected) = &expected_revoke {
                assert_eq!(&revoke_body, expected, "F1/F2 revoke");
            } else {
                expected_revoke = Some(revoke_body);
            }
        }

        let malformed = router(application_access(), repository, "workspace-a".into())
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/v1/application-access-tokens/not-a-token-id")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(malformed.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let malformed_body = to_bytes(malformed.into_body(), usize::MAX).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&malformed_body).unwrap()["detail"],
            "application token id must be an aat_-prefixed UUIDv7",
            "F3"
        );
    }
}
