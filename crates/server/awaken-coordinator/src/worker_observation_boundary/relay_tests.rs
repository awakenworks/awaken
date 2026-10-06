use super::*;
use awaken_session_application::SessionEnvironmentSource as _;
use awaken_session_configuration_contract::{
    SessionBaseline, SessionBaselineInputs, SessionRuntimePlacement,
};
use awaken_session_mcp_contract::{McpTarget, SessionMcpAttachmentSet};
use awaken_session_resource_contract::SessionRealizationLease;
use awaken_session_state_contract::*;
use awaken_worker_contract::{WorkerHeartbeat, WorkerRegistration};
use axum::body::Body;
use axum::http::Request;
use std::sync::atomic::{AtomicUsize, Ordering};
use tower::ServiceExt as _;

struct ExactPod {
    identity: awaken_worker_contract::WorkerIdentity,
    mode: AtomicUsize,
    reads: AtomicUsize,
}

#[async_trait]
impl crate::WorkerRelayTargetObserver for ExactPod {
    async fn ip_for(
        &self,
        worker: &awaken_worker_contract::WorkerIdentity,
    ) -> Result<std::net::IpAddr, crate::WorkerRelayTargetError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        if worker != &self.identity || self.mode.load(Ordering::SeqCst) == 1 {
            return Err(crate::WorkerRelayTargetError::InvalidIdentity);
        }
        if self.mode.load(Ordering::SeqCst) == 2 {
            return Err(crate::WorkerRelayTargetError::Unavailable);
        }
        Ok("10.1.2.3".parse().unwrap())
    }
}

async fn get(app: Router, query: &str, token: Option<&str>) -> axum::response::Response {
    let mut request = Request::get(format!("{WORKER_RELAY_DESTINATION_PATH}?{query}"));
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    app.oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

#[tokio::test]
async fn destination_boundary_requires_exact_auth_root_registry_and_pod() {
    // Causes: C1 dedicated relay token; C2 valid query; C3 published live root;
    // C4 Ready unexpired Registry incarnation; C5 exact available Pod evidence.
    // R1 all -> E1 secret-free destination with min(root, Registry) expiry.
    // R2 !C1 -> 401 before reads; R3 !C2 -> 400; R4 !C3/!C4/wrong Pod -> 404;
    // R5 missing authenticator/observer or unavailable dependency -> 503.
    // Earlier refusal rows never call the Pod port. The test composes the same
    // Session application/repository and Registry owners as product startup.
    let clock = Arc::new(awaken_lease_testkit::ManualClock::new(100));
    let host = Arc::new(
        awaken_runtime_host_testkit::volatile_host(
            Arc::new(awaken_scenario_host::EchoModel),
            "relay-test",
        )
        .await,
    );
    let environments = crate::volatile_environment_execution(clock.clone());
    let resources = awaken_resource_persistence::ephemeral(clock.clone())
        .await
        .unwrap();
    let sessions = crate::local_managed_state_with_environments(
        host.clone(),
        resources.authorities().resource_registry(),
        environments.clone(),
    )
    .await
    .sessions;
    let directory = Arc::new(awaken_worker_registry::MemoryWorkerDirectory::new());
    let registered = directory
        .register(
            WorkerRegistration {
                worker_id: "k8s:ns:worker:uid-a".into(),
                incarnation_id: "uid-a".into(),
                manifest: awaken_worker_contract::WorkerManifest::default(),
            },
            100,
            50,
        )
        .await
        .unwrap();
    let pod = Arc::new(ExactPod {
        identity: registered.snapshot.identity.clone(),
        mode: AtomicUsize::new(0),
        reads: AtomicUsize::new(0),
    });
    let holder = awaken_credential_contract::PlaintextHolder::new(
        awaken_credential_contract::PlaintextBoundary::Worker,
        "worker.example",
    );
    let credential = awaken_credential_contract::CredentialAccess::new(
        awaken_credential_contract::CredentialRef {
            id: "credential".into(),
            revision: 1,
        },
        awaken_credential_contract::CredentialMaterialSource::ControlPlaneReference,
        awaken_credential_contract::CredentialUsage::HttpHeader {
            name: "authorization".into(),
            scheme: Some("Bearer".into()),
        },
        awaken_credential_contract::CredentialExecutionPolicy::exact(
            holder.clone(),
            awaken_credential_contract::ModelExposurePolicy::VirtualOnly,
        ),
    );
    let mut mcp = SessionMcpAttachmentSet::from_initial(
        vec![awaken_session_mcp_contract::McpAttachmentDraft {
            name: "echo".into(),
            target: McpTarget::parse_http("https://mcp.example.test/").unwrap(),
            prompts_as_skills: false,
            credential: Some(credential),
            origin: awaken_session_mcp_contract::McpAttachmentOrigin::Agent,
        }],
        Some(holder.clone()),
    )
    .unwrap();
    mcp.attachments[0].state = awaken_session_mcp_contract::McpAttachmentState::Active;
    mcp.attachments[0].publication_acknowledged = true;
    mcp.attachments[0].realization = Some(awaken_session_mcp_contract::McpRealizationClaim {
        realization_id: "realization".into(),
        runtime_incarnation: "runtime".into(),
        lease_epoch: 3,
        lease_expires_at_unix_ms: 140,
        stage_idempotency_key: "stage".into(),
    });
    let attachment = mcp.attachments[0].attachment_id.0.clone();
    let mut environment = environments
        .resolve_current_for_session(
            awaken_environment_contract::BUILTIN_LOCAL_ENVIRONMENT_ID,
            None,
            &[],
        )
        .await
        .unwrap()
        .unwrap()
        .snapshot;
    environment.credential_realization.mcp_holder = holder;
    let baseline = SessionBaseline::compile(SessionBaselineInputs {
        environment,
        runtime_placement: SessionRuntimePlacement::Worker,
        mcp_authoring: Default::default(),
        agent_id: "agent".into(),
        agent_revision: None,
        model: "relay-test".into(),
        model_override: None,
        runtime: None,
        delegate_ids: vec![],
        toolsets: vec![],
        mounts: vec![],
        env: vec![],
        prompts: vec![],
        transcript_prefix: None,
    });
    let mut root = PersistedSession::frozen_with_budget(
        "relay-session",
        baseline,
        Default::default(),
        mcp,
        None,
        Default::default(),
        Default::default(),
        Default::default(),
    );
    root.realization = Some(SessionRealizationLease {
        owner: registered.snapshot.identity.worker_id.clone(),
        runtime_incarnation: "runtime".into(),
        epoch: 3,
        expires_at_unix_ms: 140,
    });
    let hash = SessionMutationPayload::Replace(root.clone()).stable_hash();
    sessions
        .session_repository_handle()
        .create(
            "owner",
            root,
            IdempotencyRecord {
                key: "create-relay".into(),
                payload_hash: hash,
            },
            vec![],
        )
        .await
        .unwrap();
    let state = DomainState {
        directory: directory.clone(),
        host,
        environments,
        sessions,
        relay_targets: Some(pod.clone()),
        clock: clock.clone(),
        authenticator: awaken_service_auth_contract::static_token_authenticator("broad-token")
            .unwrap(),
        relay_authenticator: Some(
            awaken_service_auth_contract::static_token_authenticator("relay-token").unwrap(),
        ),
    };
    let query = format!("session_id=relay-session&attachment_id={attachment}&version=1");
    let app = domain_router_with_authenticator(state.clone());
    for token in [None, Some("wrong"), Some("broad-token")] {
        assert_eq!(
            get(app.clone(), &query, token).await.status(),
            StatusCode::UNAUTHORIZED,
            "R2"
        );
    }
    assert_eq!(
        get(app.clone(), "version=0", Some("relay-token"))
            .await
            .status(),
        StatusCode::BAD_REQUEST,
        "R3"
    );
    assert_eq!(
        get(app.clone(), &query, Some("relay-token")).await.status(),
        StatusCode::NOT_FOUND,
        "R4 Starting"
    );
    assert_eq!(pod.reads.load(Ordering::SeqCst), 0, "R2-R4 before Pod read");
    directory
        .heartbeat(
            &registered.snapshot.identity,
            WorkerHeartbeat {
                sequence: 1,
                ready: true,
                in_flight: 0,
                warm_environment_shapes: Default::default(),
                credential_observations: Default::default(),
                acp_capability_observations: vec![],
            },
            100,
            50,
        )
        .await
        .unwrap();
    let accepted = get(app.clone(), &query, Some("relay-token")).await;
    assert_eq!(accepted.status(), StatusCode::OK, "R1");
    let bytes = axum::body::to_bytes(accepted.into_body(), 4096)
        .await
        .unwrap();
    let destination: awaken_worker_contract::WorkerRelayDestination =
        serde_json::from_slice(&bytes).unwrap();
    assert_eq!(destination.worker(), &registered.snapshot.identity, "R1");
    assert_eq!(destination.expires_at_unix_ms(), 140, "R1 min expiry");
    for (mode, expected) in [
        (1, StatusCode::NOT_FOUND),
        (2, StatusCode::SERVICE_UNAVAILABLE),
    ] {
        pod.mode.store(mode, Ordering::SeqCst);
        assert_eq!(
            get(app.clone(), &query, Some("relay-token")).await.status(),
            expected,
            "R4/R5 Pod"
        );
    }
    pod.mode.store(0, Ordering::SeqCst);
    for (authenticator, observer) in [(false, true), (true, false)] {
        let mut missing = state.clone();
        if !authenticator {
            missing.relay_authenticator = None;
        }
        if !observer {
            missing.relay_targets = None;
        }
        assert_eq!(
            get(
                domain_router_with_authenticator(missing),
                &query,
                Some("relay-token")
            )
            .await
            .status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "R5 uninstalled"
        );
    }
    clock.advance(40);
    assert_eq!(
        get(app, &query, Some("relay-token")).await.status(),
        StatusCode::NOT_FOUND,
        "R4 exact root expiry"
    );
}
