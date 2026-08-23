//! Coordinator verification edge for official Managed Agents Session workers.

use std::sync::Arc;

use awaken_authz_enforce::RequestTenancy;
use awaken_environment_execution_application::{
    EnvironmentExecutionApplication, EnvironmentExecutionError,
};
use awaken_iam_server::{
    CapabilityCheck, CapabilityClaims, LeaseEpoch, decode_unverified_claims, verify_capability,
};
use awaken_protocol_managed::{SESSION_WORK_SCOPE, SessionWorkCapabilityConfiguration};
use awaken_resource_contract::{MemoryActor, ResourceAccess};
use awaken_session_contract::work_queue::VerifiedSessionWorkLease;
use awaken_session_contract::{ManagedSessionRepository, PersistedSession, ResolvedInputSource};
use awaken_tenancy::WorkspaceScope;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::Engine as _;

const MAX_CAPABILITY_BYTES: usize = 16 * 1024;

/// The one immutable verification composition. It references the canonical IAM
/// key authority, WorkQueue lease authority, and Session aggregate repository;
/// it owns no replicated authorization state.
#[derive(Clone)]
pub struct SessionWorkCapabilityGuard {
    capability: SessionWorkCapabilityConfiguration,
    environments: Arc<EnvironmentExecutionApplication>,
    sessions: Arc<dyn ManagedSessionRepository>,
}

impl SessionWorkCapabilityGuard {
    #[must_use]
    pub fn new(
        capability: SessionWorkCapabilityConfiguration,
        environments: Arc<EnvironmentExecutionApplication>,
        sessions: Arc<dyn ManagedSessionRepository>,
    ) -> Self {
        Self {
            capability,
            environments,
            sessions,
        }
    }

    /// Blanket layer for a protected router. Ordinary IAM credentials pass to
    /// the existing guard unchanged; capability-shaped JWTs are terminally
    /// accepted or rejected here and can never fall back to another credential
    /// family.
    #[must_use]
    pub fn protect(self, router: axum::Router) -> axum::Router {
        router.layer(axum::middleware::from_fn_with_state(
            Arc::new(self),
            session_work_capability_guard,
        ))
    }
}

enum PresentedCredential<'a> {
    Ordinary,
    Capability(&'a str),
    InvalidJwt,
}

async fn session_work_capability_guard(
    State(state): State<Arc<SessionWorkCapabilityGuard>>,
    mut request: Request,
    next: Next,
) -> Response {
    let Some(token) = presented_token(request.headers()) else {
        return next.run(request).await;
    };
    let token = match classify_token(token) {
        PresentedCredential::Ordinary => return next.run(request).await,
        PresentedCredential::InvalidJwt => return StatusCode::UNAUTHORIZED.into_response(),
        PresentedCredential::Capability(token) => token,
    };

    let hint: CapabilityClaims = match decode_unverified_claims(token) {
        Ok(claims) => claims,
        Err(_) => return StatusCode::UNAUTHORIZED.into_response(),
    };
    let session = match state.sessions.get(&hint.sub).await {
        Ok(session) => session,
        Err(awaken_session_contract::SessionRepositoryError::NotFound) => {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let now_ms = unix_now_ms();
    let lease = match state
        .environments
        .current_session_work_lease(session.environment_id(), &session.session_id, now_ms)
        .await
    {
        Ok(Some(lease)) => lease,
        Ok(None) | Err(EnvironmentExecutionError::EnvironmentNotFound) => {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let now = match i64::try_from(now_ms / 1_000) {
        Ok(now) => now,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let claims = match verify_capability(
        token,
        &state.capability.authority().jwks(),
        CapabilityCheck {
            audience: state.capability.audience(),
            epoch: LeaseEpoch(lease.epoch),
            now,
        },
    ) {
        Ok(claims) => claims,
        Err(_) => return StatusCode::UNAUTHORIZED.into_response(),
    };
    if claims.iss != state.capability.issuer()
        || claims.sub != session.session_id
        || claims.scope.as_slice() != [SESSION_WORK_SCOPE]
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !route_is_authorized(request.method(), request.uri().path(), &session, &lease) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let workspace = match state.sessions.owner(&session.session_id).await {
        Ok(workspace) if !workspace.trim().is_empty() => workspace,
        Ok(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
        Err(awaken_session_contract::SessionRepositoryError::NotFound) => {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    request
        .extensions_mut()
        .insert(VerifiedSessionWorkLease(lease));
    request
        .extensions_mut()
        .insert(WorkspaceScope(workspace.clone()));
    request.extensions_mut().insert(RequestTenancy {
        workspace_id: workspace,
    });
    request.extensions_mut().insert(
        awaken_protocol_managed::types::memory::AuthenticatedMemoryActor(
            MemoryActor::SessionActor {
                session_id: session.session_id,
            },
        ),
    );
    next.run(request).await
}

fn presented_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .and_then(|value| value.split_once(' '))
        .filter(|(scheme, token)| scheme.eq_ignore_ascii_case("bearer") && !token.trim().is_empty())
        .map(|(_, token)| token.trim())
        .or_else(|| {
            headers
                .get("x-api-key")
                .and_then(|value| value.to_str().ok())
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
}

fn classify_token(token: &str) -> PresentedCredential<'_> {
    if token.split('.').count() != 3 {
        return PresentedCredential::Ordinary;
    }
    if token.len() > MAX_CAPABILITY_BYTES {
        return PresentedCredential::InvalidJwt;
    }
    let Some(header) = token.split('.').next() else {
        return PresentedCredential::InvalidJwt;
    };
    let Ok(decoded) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(header) else {
        return PresentedCredential::InvalidJwt;
    };
    let Ok(header) = serde_json::from_slice::<serde_json::Value>(&decoded) else {
        return PresentedCredential::InvalidJwt;
    };
    if header.get("typ").and_then(serde_json::Value::as_str) == Some("cap+jwt") {
        PresentedCredential::Capability(token)
    } else {
        PresentedCredential::Ordinary
    }
}

fn route_is_authorized(
    method: &Method,
    path: &str,
    session: &PersistedSession,
    lease: &awaken_session_contract::work_queue::SessionWorkLease,
) -> bool {
    let segments = path
        .trim_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    match segments.as_slice() {
        ["v1", "sessions", session_id] => {
            *method == Method::GET && *session_id == session.session_id
        }
        ["v1", "sessions", session_id, "events"] => {
            matches!(*method, Method::GET | Method::POST) && *session_id == session.session_id
        }
        ["v1", "sessions", session_id, "events", "stream"] => {
            *method == Method::GET && *session_id == session.session_id
        }
        [
            "v1",
            "environments",
            environment_id,
            "work",
            work_id,
            operation,
        ] => {
            *method == Method::POST
                && matches!(*operation, "heartbeat" | "stop")
                && *environment_id == lease.environment_id
                && *work_id == lease.work_id
        }
        ["v1", "skills", skill_id, "versions", version]
        | ["v1", "skills", skill_id, "versions", version, "content"] => {
            *method == Method::GET
                && session.resources.active.skills().iter().any(|skill| {
                    skill.kind == awaken_agent_contract::AgentSkillKind::Custom
                        && skill.skill_id == *skill_id
                        && skill.version.to_string() == *version
                })
        }
        ["v1", "memory_stores", store_id, "memories"] => memory_access(session, store_id)
            .is_some_and(|access| {
                *method == Method::GET
                    || (*method == Method::POST && access == ResourceAccess::ReadWrite)
            }),
        ["v1", "memory_stores", store_id, "memories", _memory_id] => {
            memory_access(session, store_id).is_some_and(|access| {
                *method == Method::GET
                    || (matches!(*method, Method::POST | Method::DELETE)
                        && access == ResourceAccess::ReadWrite)
            })
        }
        _ => false,
    }
}

fn memory_access(session: &PersistedSession, store_id: &str) -> Option<ResourceAccess> {
    session
        .resources
        .active
        .inputs()
        .iter()
        .find_map(|input| match &input.source {
            ResolvedInputSource::MemoryStore {
                memory_store_id, ..
            } if memory_store_id.as_str() == store_id => Some(input.access),
            _ => None,
        })
}

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use async_trait::async_trait;
    use awaken_iam_server::{AccessTokenAuthority, LocalSeedSigner};
    use awaken_resource_contract::{
        BindingId, ConfigVersion, MemoryStoreConfigVersion, MemoryStoreId, RetentionPolicy,
    };
    use awaken_session_contract::{
        EnvironmentFingerprint, EnvironmentSnapshot, ManagedLifecycleFact, ResolvedInput,
        ResolvedSessionResources, ResolvedSkillBinding, SessionBaseline, SessionBaselineInputs,
        SessionMcpAuthoringContext, SessionNetworkPolicy, SessionResourceState,
        SessionRuntimePlacement,
    };
    use axum::body::Body;
    use axum::extract::Extension;
    use axum::http::Request as HttpRequest;
    use tower::ServiceExt;

    use super::*;

    fn session_fixture() -> PersistedSession {
        session_fixture_in("env-1")
    }

    fn session_fixture_in(environment_id: &str) -> PersistedSession {
        let baseline = SessionBaseline::compile(SessionBaselineInputs {
            environment: EnvironmentSnapshot {
                environment_id: environment_id.into(),
                revision: awaken_environment_contract::EnvironmentRevision(1),
                self_hosted: true,
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
            runtime_placement: SessionRuntimePlacement::Worker,
            mcp_authoring: SessionMcpAuthoringContext::default(),
            agent_id: "agent-1".into(),
            agent_revision: Some(1),
            model: "model-1".into(),
            model_override: None,
            runtime: None,
            delegate_ids: Vec::new(),
            toolsets: Vec::new(),
            mounts: Vec::new(),
            env: Vec::new(),
            prompts: Vec::new(),
            transcript_prefix: None,
        });
        let memory = |id: &str, access| ResolvedInput {
            binding_id: BindingId::from(format!("binding-{id}")),
            source: ResolvedInputSource::MemoryStore {
                memory_store_id: MemoryStoreId::from(id),
                config: MemoryStoreConfigVersion {
                    memory_store_id: MemoryStoreId::from(id),
                    version: ConfigVersion::INITIAL,
                    retention_policy: RetentionPolicy::default(),
                },
            },
            mount_path: format!("/memories/{id}"),
            access,
            instructions: None,
        };
        let resources = ResolvedSessionResources::try_new(
            vec![
                memory("mem-ro", ResourceAccess::ReadOnly),
                memory("mem-rw", ResourceAccess::ReadWrite),
            ],
            vec![ResolvedSkillBinding {
                kind: awaken_agent_contract::AgentSkillKind::Custom,
                skill_id: "skill-1".into(),
                version: 3,
                bundle_sha256: "a".repeat(64),
            }],
        )
        .unwrap();
        PersistedSession::frozen_with_budget(
            "session-1",
            baseline,
            SessionResourceState::from_active(resources),
            Default::default(),
            None,
            Default::default(),
            Default::default(),
            Default::default(),
        )
    }

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
                    "read-only fixture".into(),
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
                    "read-only fixture".into(),
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
            Ok(Default::default())
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

    async fn capability_context(
        Extension(proof): Extension<VerifiedSessionWorkLease>,
        Extension(workspace): Extension<WorkspaceScope>,
        Extension(actor): Extension<
            awaken_protocol_managed::types::memory::AuthenticatedMemoryActor,
        >,
    ) -> StatusCode {
        assert_eq!(proof.0.session_id, "session-1");
        assert_eq!(workspace.0, "workspace-1");
        assert!(matches!(
            actor.0,
            MemoryActor::SessionActor { ref session_id } if session_id == "session-1"
        ));
        StatusCode::OK
    }

    #[tokio::test]
    async fn middleware_verifies_crypto_live_lease_and_request_context_end_to_end() {
        // Cause/effect graph: C1 signature/issuer/audience/scope/subject valid;
        // C2 lease live at exact epoch; C3 route/resource allowed; C4 token is
        // tampered, cross-resource, or stale after release/reclaim. Effects: E1
        // stamp exact lease/workspace/Session actor and serve, E2 cryptographic
        // or stale authority returns 401, E3 valid-but-disallowed authority 403.
        // Decision rules: M1=C1+C2+C3=>E1; M2=!C1|!C2=>E2;
        // M3=C1+C2+!C3=>E3. This crosses real IAM signing, WorkQueue state,
        // Environment application and Session repository boundaries.
        use awaken_session_application::SessionEnvironmentSource;

        let (authoring, environments) =
            awaken_protocol_managed::test_support::environment_components();
        let created = authoring
            .application()
            .create(awaken_environment_contract::CreateEnvironmentCommand {
                command_id: "capability-test-create".into(),
                name: "capability-env".into(),
                description: None,
                metadata: Default::default(),
                scope: None,
                config: awaken_environment_contract::EnvironmentConfig::SelfHosted,
            })
            .await
            .unwrap();
        let health = environments
            .claim_work(&created.id, "owner", "poller", 1, None)
            .await
            .unwrap()
            .unwrap();
        environments
            .stop_work(&created.id, &health.item.id, "owner")
            .await
            .unwrap();
        SessionEnvironmentSource::enqueue_session_work(
            environments.as_ref(),
            &created.id,
            "session-1",
        )
        .await
        .unwrap();
        let claimed = environments
            .claim_work(&created.id, "owner-1", "poller", unix_now_ms(), None)
            .await
            .unwrap()
            .unwrap();
        let lease = claimed.session_lease.unwrap();
        let authority = AccessTokenAuthority::new(LocalSeedSigner::new("session-work", [0x71; 32]));
        let capability = SessionWorkCapabilityConfiguration::new(
            authority,
            "urn:test:coordinator",
            "managed-session-worker",
            600,
        )
        .unwrap();
        let token = capability.mint(&lease, unix_now_ms()).await.unwrap();
        let sessions: Arc<dyn ManagedSessionRepository> = Arc::new(TestSessions {
            rows: HashMap::from([(
                "session-1".into(),
                ("workspace-1".into(), session_fixture_in(&created.id)),
            )]),
        });
        let app = SessionWorkCapabilityGuard::new(capability, environments.clone(), sessions)
            .protect(axum::Router::new().fallback(capability_context));
        let call = |path: &'static str, token: String| {
            let app = app.clone();
            async move {
                app.oneshot(
                    HttpRequest::builder()
                        .method(Method::GET)
                        .uri(path)
                        .header("x-api-key", token)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
                .status()
            }
        };
        assert_eq!(
            call("/v1/sessions/session-1", token.clone()).await,
            StatusCode::OK,
            "M1/E1"
        );
        assert_eq!(
            call("/v1/sessions/session-2", token.clone()).await,
            StatusCode::FORBIDDEN,
            "M3/E3"
        );
        let mut tampered = token.clone().into_bytes();
        let last = tampered.len() - 1;
        tampered[last] = if tampered[last] == b'a' { b'b' } else { b'a' };
        assert_eq!(
            call(
                "/v1/sessions/session-1",
                String::from_utf8(tampered).unwrap()
            )
            .await,
            StatusCode::UNAUTHORIZED,
            "M2/E2"
        );
        assert!(environments.release_claim(&lease).await.unwrap());
        let reclaimed = environments
            .claim_work(&created.id, "owner-2", "poller", unix_now_ms(), None)
            .await
            .unwrap()
            .unwrap()
            .session_lease
            .unwrap();
        assert!(reclaimed.epoch > lease.epoch);
        assert_eq!(
            call("/v1/sessions/session-1", token).await,
            StatusCode::UNAUTHORIZED,
            "M2 stale epoch/E2"
        );
    }

    #[test]
    fn route_authorization_is_the_resource_derived_decision_table() {
        // Cause/effect graph: C1 route family; C2 Session/Environment/Work
        // coordinate match; C3 Resource attached; C4 exact Skill version; C5
        // Memory access mode; C6 HTTP mutation. Effects: E1 allow only official
        // worker calls for this Session, E2 deny cross-resource/cross-session,
        // E3 read-only Memory cannot mutate, E4 no admin/list/latest alias can
        // widen a frozen resource pin.
        let session = session_fixture();
        let lease = awaken_session_contract::work_queue::SessionWorkLease {
            work_id: "work-1".into(),
            environment_id: "env-1".into(),
            session_id: "session-1".into(),
            owner: "owner-1".into(),
            epoch: 4,
            expires_at_unix_ms: u64::MAX,
        };
        let cases = [
            ("A1", Method::GET, "/v1/sessions/session-1", true),
            ("A2", Method::GET, "/v1/sessions/session-2", false),
            ("A3", Method::POST, "/v1/sessions/session-1/events", true),
            (
                "A4",
                Method::GET,
                "/v1/sessions/session-1/events/stream",
                true,
            ),
            ("A5", Method::DELETE, "/v1/sessions/session-1/events", false),
            (
                "A6",
                Method::POST,
                "/v1/environments/env-1/work/work-1/heartbeat",
                true,
            ),
            (
                "A7",
                Method::POST,
                "/v1/environments/env-2/work/work-1/stop",
                false,
            ),
            ("A8", Method::GET, "/v1/skills/skill-1/versions/3", true),
            (
                "A9",
                Method::GET,
                "/v1/skills/skill-1/versions/3/content",
                true,
            ),
            (
                "A10",
                Method::GET,
                "/v1/skills/skill-1/versions/latest",
                false,
            ),
            ("A11", Method::GET, "/v1/skills/skill-1/versions", false),
            (
                "A12",
                Method::GET,
                "/v1/memory_stores/mem-ro/memories",
                true,
            ),
            (
                "A13",
                Method::POST,
                "/v1/memory_stores/mem-ro/memories",
                false,
            ),
            (
                "A14",
                Method::POST,
                "/v1/memory_stores/mem-rw/memories",
                true,
            ),
            (
                "A15",
                Method::DELETE,
                "/v1/memory_stores/mem-rw/memories/m-1",
                true,
            ),
            (
                "A16",
                Method::GET,
                "/v1/memory_stores/other/memories",
                false,
            ),
            (
                "A17",
                Method::GET,
                "/v1/memory_stores/mem-rw/memory_versions",
                false,
            ),
        ];
        for (rule, method, path, expected) in cases {
            assert_eq!(
                route_is_authorized(&method, path, &session, &lease),
                expected,
                "{rule}"
            );
        }
    }

    #[test]
    fn capability_family_detection_never_falls_back_after_jwt_shape_failure() {
        // Causes: C1 opaque key; C2 well-formed non-capability JWT; C3 cap+jwt;
        // C4 malformed/oversized JWT. Effects: E1 C1/C2 remain owned by normal
        // IAM; E2 C3 enters capability verification; E3 C4 is terminally
        // unauthorized and cannot be retried as an API key.
        let encode =
            |json: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json.as_bytes());
        assert!(
            matches!(
                classify_token("ordinary-key"),
                PresentedCredential::Ordinary
            ),
            "D1"
        );
        let access = format!("{}.e30.sig", encode(r#"{"typ":"at+jwt"}"#));
        assert!(
            matches!(classify_token(&access), PresentedCredential::Ordinary),
            "D2"
        );
        let capability = format!("{}.e30.sig", encode(r#"{"typ":"cap+jwt"}"#));
        assert!(
            matches!(
                classify_token(&capability),
                PresentedCredential::Capability(_)
            ),
            "D3"
        );
        assert!(
            matches!(
                classify_token("%%%.e30.sig"),
                PresentedCredential::InvalidJwt
            ),
            "D4"
        );
        let oversized = format!("{}.e30.sig", "a".repeat(MAX_CAPABILITY_BYTES));
        assert!(
            matches!(classify_token(&oversized), PresentedCredential::InvalidJwt),
            "D4"
        );
    }
}
