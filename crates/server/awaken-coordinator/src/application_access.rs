//! Coordinator API for issuing narrow, short-lived application credentials.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use awaken_authz_enforce::{ApplicationAccessStore, ApplicationGrant, ApplicationThreadBinding};
use awaken_session_contract::ManagedSessionRepository;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, post};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};

static TOKEN_SEQUENCE: AtomicU64 = AtomicU64::new(1);
const DEFAULT_TTL_SECONDS: u64 = 300;
const MAX_TTL_SECONDS: u64 = 900;

#[derive(Debug, Deserialize)]
pub struct CreateApplicationToken {
    pub authority_id: String,
    pub application_scope: String,
    #[serde(default)]
    pub actor_key: Option<String>,
    pub protocols: Vec<String>,
    pub operations: Vec<String>,
    pub thread_bindings: Vec<CreateApplicationThreadBinding>,
    #[serde(default = "default_ttl")]
    pub expires_in_seconds: u64,
}

#[derive(Debug, Deserialize)]
pub struct CreateApplicationThreadBinding {
    pub external_thread_id: String,
    pub managed_session_id: String,
}

#[derive(Debug, Serialize)]
pub struct IssuedApplicationToken {
    pub id: String,
    pub object: &'static str,
    pub token_type: &'static str,
    pub access_token: String,
    pub expires_at: String,
    pub application_scope: String,
    pub protocols: Vec<String>,
    pub operations: Vec<String>,
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
    if let Err(detail) = validate(&request) {
        return problem(StatusCode::UNPROCESSABLE_ENTITY, detail);
    }
    let workspace_id = workspace
        .map(|Extension(scope)| scope.0)
        .unwrap_or(state.default_workspace);
    let permits_run = request
        .operations
        .iter()
        .any(|operation| operation == "thread.run");
    let mut thread_bindings = HashMap::new();
    for requested in &request.thread_bindings {
        let owner = match state.sessions.owner(&requested.managed_session_id).await {
            Ok(owner) => owner,
            Err(awaken_session_contract::SessionRepositoryError::NotFound) => {
                return problem(StatusCode::NOT_FOUND, "bound Managed Session not found");
            }
            Err(error) => {
                return problem(
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!("Managed Session repository unavailable: {error}"),
                );
            }
        };
        if owner != workspace_id {
            return problem(StatusCode::NOT_FOUND, "bound Managed Session not found");
        }
        let session = match state.sessions.get(&requested.managed_session_id).await {
            Ok(session) => session,
            Err(awaken_session_contract::SessionRepositoryError::NotFound) => {
                return problem(StatusCode::NOT_FOUND, "bound Managed Session not found");
            }
            Err(error) => {
                return problem(
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!("Managed Session repository unavailable: {error}"),
                );
            }
        };
        let Some(agent_id) = session.agent_id() else {
            return problem(
                StatusCode::CONFLICT,
                "bound Managed Session baseline is not frozen",
            );
        };
        if permits_run && session.is_terminal() {
            return problem(
                StatusCode::CONFLICT,
                "bound Managed Session is not runnable",
            );
        }
        thread_bindings.insert(
            requested.external_thread_id.clone(),
            ApplicationThreadBinding {
                managed_session_id: requested.managed_session_id.clone(),
                agent_id: agent_id.to_string(),
            },
        );
    }
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default();
    let expires_ms = now_ms.saturating_add(request.expires_in_seconds.saturating_mul(1000));
    let expires_at = awaken_session_contract::epoch_millis_to_rfc3339(expires_ms);
    let id = format!(
        "aat_{}_{}",
        now_ms,
        TOKEN_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    let grant = ApplicationGrant {
        authority_id: request.authority_id,
        application_scope: request.application_scope.clone(),
        actor_key: request.actor_key,
        protocols: request.protocols.iter().cloned().collect::<HashSet<_>>(),
        operations: request.operations.iter().cloned().collect::<HashSet<_>>(),
        thread_bindings,
    };
    match state
        .store
        .mint(id.clone(), workspace_id, Some(expires_at.clone()), grant)
    {
        Ok(access_token) => (
            StatusCode::CREATED,
            Json(IssuedApplicationToken {
                id,
                object: "application_access_token",
                token_type: "Bearer",
                access_token,
                expires_at,
                application_scope: request.application_scope,
                protocols: request.protocols,
                operations: request.operations,
            }),
        )
            .into_response(),
        Err(error) => problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("could not issue application token: {error}"),
        ),
    }
}

async fn revoke(State(state): State<ApplicationTokenState>, Path(id): Path<String>) -> Response {
    match state.store.revoke(&id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => problem(StatusCode::NOT_FOUND, "application token not found"),
    }
}

fn validate(request: &CreateApplicationToken) -> Result<(), &'static str> {
    if request.authority_id.trim().is_empty() || request.application_scope.trim().is_empty() {
        return Err("authority_id and application_scope are required");
    }
    if request.expires_in_seconds == 0 || request.expires_in_seconds > MAX_TTL_SECONDS {
        return Err("expires_in_seconds must be between 1 and 900");
    }
    if request.protocols.is_empty()
        || request
            .protocols
            .iter()
            .any(|protocol| !matches!(protocol.as_str(), "ai-sdk" | "ag-ui"))
        || request.protocols.iter().collect::<HashSet<_>>().len() != request.protocols.len()
    {
        return Err("protocols must be unique and may contain only ai-sdk and ag-ui");
    }
    if request.operations.is_empty()
        || request
            .operations
            .iter()
            .any(|operation| !matches!(operation.as_str(), "thread.run" | "thread.messages.read"))
        || request.operations.iter().collect::<HashSet<_>>().len() != request.operations.len()
    {
        return Err(
            "operations must be unique and may contain only thread.run and thread.messages.read",
        );
    }
    if request.thread_bindings.is_empty()
        || request.thread_bindings.iter().any(|binding| {
            binding.external_thread_id.trim().is_empty()
                || binding.managed_session_id.trim().is_empty()
        })
    {
        return Err("thread_bindings must contain at least one complete binding");
    }
    let external_ids = request
        .thread_bindings
        .iter()
        .map(|binding| binding.external_thread_id.as_str())
        .collect::<HashSet<_>>();
    let session_ids = request
        .thread_bindings
        .iter()
        .map(|binding| binding.managed_session_id.as_str())
        .collect::<HashSet<_>>();
    if external_ids.len() != request.thread_bindings.len()
        || session_ids.len() != request.thread_bindings.len()
    {
        return Err("thread_bindings must be one-to-one and unique");
    }
    Ok(())
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

    struct TestSessions {
        rows: HashMap<String, (String, PersistedSession)>,
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
            awaken_session_contract::SessionRevision,
            awaken_session_contract::SessionRepositoryError,
        > {
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
            activity_epoch: 0,
            running_interval: None,
            runtime_active_millis: 0,
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
                    environment: baseline.environment,
                    runtime_placement: awaken_session_contract::SessionRuntimePlacement::Local,
                    agent_id: baseline.agent_id,
                    agent_revision: baseline.agent_revision,
                    model_override: baseline.model_override,
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

    fn sessions(rows: Vec<(&str, &str, PersistedSession)>) -> Arc<TestSessions> {
        Arc::new(TestSessions {
            rows: rows
                .into_iter()
                .map(|(id, owner, session)| (id.to_string(), (owner.to_string(), session)))
                .collect(),
        })
    }

    fn issue_body(session_id: &str, operations: Value) -> Value {
        json!({
            "authority_id": "shop-backend",
            "application_scope": "project-42",
            "actor_key": "opaque-user",
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
        let store = Arc::new(ApplicationAccessStore::new());
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
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        let token = body["access_token"].as_str().unwrap();
        let identity = store.authenticate(token).unwrap();
        assert_eq!(identity.workspace_id, "workspace-a");
        assert_eq!(identity.grant.application_scope, "project-42");
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
                Arc::new(ApplicationAccessStore::new()),
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
            Arc::new(ApplicationAccessStore::new()),
            repository.clone(),
            issue_body("sesn_preparing", json!(["thread.run"])),
        )
        .await;
        assert_eq!(preparing.status(), StatusCode::CONFLICT);

        let terminal_run = issue(
            Arc::new(ApplicationAccessStore::new()),
            repository.clone(),
            issue_body("sesn_terminal", json!(["thread.run"])),
        )
        .await;
        assert_eq!(terminal_run.status(), StatusCode::CONFLICT);

        let terminal_read = issue(
            Arc::new(ApplicationAccessStore::new()),
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
                "authority_id": "shop-backend",
                "application_scope": "project-42",
                "protocols": ["unknown"],
                "operations": ["thread.run"],
                "thread_bindings": [{
                    "external_thread_id": "customer-thread",
                    "managed_session_id": "sesn_1"
                }]
            }),
            json!({
                "authority_id": "shop-backend",
                "application_scope": "project-42",
                "protocols": ["ai-sdk"],
                "operations": ["thread.read"],
                "thread_bindings": [{
                    "external_thread_id": "customer-thread",
                    "managed_session_id": "sesn_1"
                }]
            }),
            json!({
                "authority_id": "shop-backend",
                "application_scope": "project-42",
                "protocols": ["ai-sdk"],
                "operations": ["thread.run"],
                "thread_bindings": [
                    { "external_thread_id": "one", "managed_session_id": "sesn_1" },
                    { "external_thread_id": "two", "managed_session_id": "sesn_1" }
                ]
            }),
            json!({
                "authority_id": "shop-backend",
                "application_scope": "project-42",
                "protocols": ["ai-sdk", "ai-sdk"],
                "operations": ["thread.run"],
                "thread_bindings": [{
                    "external_thread_id": "customer-thread",
                    "managed_session_id": "sesn_1"
                }]
            }),
        ];
        for body in cases {
            let response = issue(
                Arc::new(ApplicationAccessStore::new()),
                repository.clone(),
                body,
            )
            .await;
            assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        }
    }
}
