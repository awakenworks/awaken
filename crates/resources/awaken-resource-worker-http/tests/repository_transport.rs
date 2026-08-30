//! Repository binding boundary tests over real HTTP.

use awaken_run_ingress_testkit::worker_http as support;

use std::sync::{Arc, Mutex};

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_resource_contract::RepositoryBindingVerifier as _;
use awaken_resource_contract::{ConfigVersion, LiveResourceBindingVerifier, ResourceRegistryError};
use awaken_resource_worker_http::HttpRepositoryBindingVerifier;
use awaken_resource_worker_http::{
    RepositoryTransportAuthority, RepositoryTransportAuthorization, RepositoryTransportAuthorizer,
    WorkerRepositoryBindingService, worker_repository_binding_router,
};
use awaken_run_ingress::{
    DispatchQueue as _, MemoryDispatchStore, RunClaim, RunDispatch, WorkerIdentity,
};
use awaken_worker_transport_security::{HeaderWorkerAuthenticator, WorkerUpstream};

struct ExactRepositoryRegistry {
    active: bool,
}

#[derive(Clone)]
struct ExactTerminalPublicationControl {
    workspace_id: String,
    command: awaken_session_contract::SessionRepositoryPublicationCommand,
    lease: Arc<Mutex<awaken_session_contract::SessionRealizationLease>>,
}

#[async_trait::async_trait]
impl awaken_session_contract::SessionRealizationControl for ExactTerminalPublicationControl {
    async fn begin_session_realization(
        &self,
        _command: awaken_session_contract::BeginSessionRealization,
    ) -> Result<
        awaken_session_contract::SessionRealizationDirective,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        Err(
            awaken_session_contract::SessionRealizationControlFailure::Invalid(
                "unused test phase".into(),
            ),
        )
    }

    async fn activate_session_realization(
        &self,
        _command: awaken_session_contract::ActivateSessionRealization,
    ) -> Result<
        awaken_session_contract::SessionRealizationDirective,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        Err(
            awaken_session_contract::SessionRealizationControlFailure::Invalid(
                "unused test phase".into(),
            ),
        )
    }

    async fn acknowledge_session_realization(
        &self,
        _command: awaken_session_contract::AcknowledgeSessionRealization,
    ) -> Result<
        awaken_session_contract::SessionRealizationDirective,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        Err(
            awaken_session_contract::SessionRealizationControlFailure::Invalid(
                "unused test phase".into(),
            ),
        )
    }

    async fn fail_session_realization(
        &self,
        _command: awaken_session_contract::FailSessionRealization,
    ) -> Result<(), awaken_session_contract::SessionRealizationControlFailure> {
        Err(
            awaken_session_contract::SessionRealizationControlFailure::Invalid(
                "unused test phase".into(),
            ),
        )
    }

    async fn terminal_repository_publication_command(
        &self,
        session_id: &str,
        asserted_lease: &awaken_session_contract::SessionRealizationLease,
    ) -> Result<
        Option<awaken_session_contract::SessionRepositoryPublicationProjection>,
        awaken_session_contract::SessionRealizationControlFailure,
    > {
        let current_lease = self.lease.lock().unwrap().clone();
        if session_id != self.command.session_id
            || !awaken_session_contract::realization_lease_authorizes(
                &current_lease,
                asserted_lease,
                support::unix_now_ms(),
            )
        {
            return Err(awaken_session_contract::SessionRealizationControlFailure::StaleOwnership);
        }
        Ok(Some(
            awaken_session_contract::SessionRepositoryPublicationProjection {
                workspace_id: self.workspace_id.clone(),
                command: self.command.clone(),
                current_lease,
            },
        ))
    }
}

fn terminal_publication_command(
    session_id: &str,
) -> awaken_session_contract::SessionRepositoryPublicationCommand {
    let intent = serde_json::from_value(serde_json::json!({
        "input": {
            "binding_id": "repository-binding",
            "source": {
                "kind": "repository",
                "repository_id": "repository-exact",
                "config": {
                    "repository_id": "repository-exact",
                    "version": 1,
                    "remote_url": "https://git.invalid/exact.git",
                    "initial_branch": "main"
                }
            },
            "mount_path": "/workspace/repository",
            "access": "read_write"
        },
        "expectation": {
            "branch": "awf/work",
            "commit": "0123456789abcdef0123456789abcdef01234567"
        }
    }))
    .expect("terminal publication intent");
    let mut operation = awaken_session_contract::SessionCleanupOperation::default();
    operation
        .request_with_publication(session_id, intent)
        .expect("terminal publication fence");
    operation
        .freeze_targets(session_id, [], 0, 0)
        .expect("terminal publication target");
    operation
        .publication_command(session_id)
        .expect("terminal publication projection")
        .expect("terminal publication command")
}

impl LiveResourceBindingVerifier for ExactRepositoryRegistry {
    fn verify_memory_binding(
        &self,
        _workspace_id: &str,
        _id: &str,
        _version: ConfigVersion,
    ) -> Result<(), ResourceRegistryError> {
        Err(ResourceRegistryError::NotFound("memory".into()))
    }

    fn verify_repository_binding(
        &self,
        workspace_id: &str,
        id: &str,
        version: ConfigVersion,
    ) -> Result<(), ResourceRegistryError> {
        if self.active
            && workspace_id == "workspace-repository"
            && id == "repository-exact"
            && version == ConfigVersion::INITIAL
        {
            Ok(())
        } else {
            Err(ResourceRegistryError::NotFound(id.into()))
        }
    }
}

fn resources() -> awaken_session_contract::ResolvedSessionResources {
    awaken_session_contract::ResolvedSessionResources::try_new(
        vec![awaken_session_contract::ResolvedInput {
            binding_id: awaken_resource_contract::BindingId::from("repository-binding"),
            source: awaken_session_contract::ResolvedInputSource::Repository {
                repository_id: awaken_resource_contract::RepositoryId::from("repository-exact"),
                config: awaken_resource_contract::RepositoryConfigVersion {
                    repository_id: "repository-exact".into(),
                    version: ConfigVersion::INITIAL,
                    remote_url: "https://git.invalid/exact.git".into(),
                    credential_binding: None,
                    initial_branch: Some("main".into()),
                    initial_commit: None,
                    clone_policy: Default::default(),
                },
                credential: None,
            },
            mount_path: "/workspace/repository".into(),
            access: awaken_resource_contract::ResourceAccess::ReadWrite,
            instructions: None,
        }],
        Vec::new(),
    )
    .unwrap()
}

async fn claimed_dispatch(dispatch: &Arc<MemoryDispatchStore>, owner: &str) -> RunClaim {
    let request = RunDispatch::new(support::activation("repository"))
        .with_execution_scope(awaken_tenancy::ExecutionScopeRef(
            awaken_tenancy::ScopeId::from("workspace-repository"),
        ))
        .with_session_resources(awaken_run_ingress::SessionResourceEnvelope::new(
            "workspace-repository",
            serde_json::to_string(&resources()).unwrap(),
        ));
    dispatch.enqueue(request).await.unwrap();
    let claimed = dispatch
        .claim(owner, 60_000, support::unix_now_ms(), &Default::default())
        .await
        .unwrap()
        .unwrap();
    RunClaim::from(&claimed.lease)
}

#[derive(Default)]
struct GatewayAuthorizer(Mutex<Vec<RepositoryTransportAuthorization>>);

#[async_trait::async_trait]
impl RepositoryTransportAuthorizer for GatewayAuthorizer {
    async fn authorize(
        &self,
        request: RepositoryTransportAuthorization,
    ) -> Result<
        awaken_resource_contract::RepositoryTransport,
        awaken_resource_contract::RepositoryBindingVerifierError,
    > {
        let authority_expiry = match &request.authority {
            RepositoryTransportAuthority::Run {
                claim_expires_ms, ..
            } => *claim_expires_ms,
            RepositoryTransportAuthority::TerminalPublication { lease, .. } => {
                lease.expires_at_unix_ms
            }
        };
        self.0.lock().unwrap().push(request);
        Ok(
            awaken_resource_contract::RepositoryTransport::GatewayMediated {
                remote_url: "https://gateway.invalid/git/repository-exact".into(),
                capability: awaken_resource_contract::RepositoryGatewayCapability::new(
                    "repository-capability",
                )?,
                expires_at_unix_ms: Some(
                    awaken_resource_contract::RepositoryGatewayCapabilityExpiry::new(
                        authority_expiry.saturating_sub(1),
                    )?,
                ),
            },
        )
    }
}

struct FixedExpiryAuthorizer(Option<u64>);

#[async_trait::async_trait]
impl RepositoryTransportAuthorizer for FixedExpiryAuthorizer {
    async fn authorize(
        &self,
        _request: RepositoryTransportAuthorization,
    ) -> Result<
        awaken_resource_contract::RepositoryTransport,
        awaken_resource_contract::RepositoryBindingVerifierError,
    > {
        Ok(
            awaken_resource_contract::RepositoryTransport::GatewayMediated {
                remote_url: "https://gateway.invalid/git/repository-exact".into(),
                capability: awaken_resource_contract::RepositoryGatewayCapability::new(
                    "repository-capability",
                )?,
                expires_at_unix_ms: self
                    .0
                    .map(awaken_resource_contract::RepositoryGatewayCapabilityExpiry::new)
                    .transpose()?,
            },
        )
    }
}

struct DeniedAuthorizer;

#[async_trait::async_trait]
impl RepositoryTransportAuthorizer for DeniedAuthorizer {
    async fn authorize(
        &self,
        _request: RepositoryTransportAuthorization,
    ) -> Result<
        awaken_resource_contract::RepositoryTransport,
        awaken_resource_contract::RepositoryBindingVerifierError,
    > {
        Err(awaken_resource_contract::RepositoryBindingVerifierError::new("gateway denied"))
    }
}

struct CancellingAuthorizer {
    dispatch: Arc<MemoryDispatchStore>,
    run_id: RunId,
}

#[async_trait::async_trait]
impl RepositoryTransportAuthorizer for CancellingAuthorizer {
    async fn authorize(
        &self,
        _request: RepositoryTransportAuthorization,
    ) -> Result<
        awaken_resource_contract::RepositoryTransport,
        awaken_resource_contract::RepositoryBindingVerifierError,
    > {
        self.dispatch.cancel(&self.run_id).await.unwrap();
        Ok(
            awaken_resource_contract::RepositoryTransport::GatewayMediated {
                remote_url: "https://gateway.invalid/git/repository-exact".into(),
                capability: awaken_resource_contract::RepositoryGatewayCapability::new(
                    "repository-capability",
                )?,
                expires_at_unix_ms: None,
            },
        )
    }
}

struct RenewingTerminalAuthorizer {
    current_lease: Arc<Mutex<awaken_session_contract::SessionRealizationLease>>,
    renewal: awaken_session_contract::SessionRealizationLease,
    capability_expiry_unix_ms: u64,
}

#[async_trait::async_trait]
impl RepositoryTransportAuthorizer for RenewingTerminalAuthorizer {
    async fn authorize(
        &self,
        request: RepositoryTransportAuthorization,
    ) -> Result<
        awaken_resource_contract::RepositoryTransport,
        awaken_resource_contract::RepositoryBindingVerifierError,
    > {
        let RepositoryTransportAuthority::TerminalPublication { lease, .. } = request.authority
        else {
            return Err(
                awaken_resource_contract::RepositoryBindingVerifierError::new(
                    "renewing fixture requires terminal publication authority",
                ),
            );
        };
        let mut current_lease = self.current_lease.lock().unwrap();
        if lease != *current_lease
            || !awaken_session_contract::realization_lease_generation_authorizes(
                &self.renewal,
                &lease,
            )
        {
            return Err(
                awaken_resource_contract::RepositoryBindingVerifierError::new(
                    "renewing fixture received another terminal generation",
                ),
            );
        }
        *current_lease = self.renewal.clone();
        drop(current_lease);
        Ok(
            awaken_resource_contract::RepositoryTransport::GatewayMediated {
                remote_url: "https://gateway.invalid/git/repository-exact".into(),
                capability: awaken_resource_contract::RepositoryGatewayCapability::new(
                    "renewed-operation-capability",
                )?,
                expires_at_unix_ms: Some(
                    awaken_resource_contract::RepositoryGatewayCapabilityExpiry::new(
                        self.capability_expiry_unix_ms,
                    )?,
                ),
            },
        )
    }
}

/// Cause/effect decision table:
/// | Rule | Worker auth | live exact claim | frozen Repository binding | catalog state | Effect |
/// |---|---|---|---|---|---|
/// | R1 | valid incarnation | yes | exact Workspace/id/version | active exact version | permit Git realization |
/// | R2 | valid incarnation | yes | another id/version | any | deny before catalog validation |
/// | R3 | missing | any | any | any | HTTP 401 before claim/catalog |
/// | R4 | stale incarnation | yes | exact | active | deny before catalog validation |
/// | R5 | valid incarnation | stale epoch | exact | active | reject before catalog validation |
/// | R6 | valid incarnation | yes | exact | archived/deleted | deny before Git realization |
#[tokio::test]
async fn repository_verification_is_scope_claim_manifest_and_incarnation_fenced() {
    let (directory, identity) = support::ready_worker("worker-repository").await;
    let dispatch = Arc::new(MemoryDispatchStore::new());
    let claim = claimed_dispatch(&dispatch, &identity.lease_owner()).await;
    let service = Arc::new(
        WorkerRepositoryBindingService::new(
            Arc::new(ExactRepositoryRegistry { active: true }),
            dispatch.clone(),
            Arc::new(HeaderWorkerAuthenticator),
        )
        .with_worker_directory(directory.clone()),
    );
    let address = support::serve(worker_repository_binding_router(service)).await;
    let verifier = HttpRepositoryBindingVerifier::new(
        WorkerUpstream::new(format!("http://{address}")).with_worker_identity(identity.clone()),
    );

    verifier
        .verify(
            "workspace-repository",
            "repository-exact",
            ConfigVersion::INITIAL,
            Some(&claim),
        )
        .await
        .expect("R1 exact Repository binding");

    let denied = verifier
        .verify(
            "workspace-repository",
            "repository-other",
            ConfigVersion::INITIAL,
            Some(&claim),
        )
        .await
        .expect_err("R2 non-frozen Repository must be denied");
    assert!(denied.to_string().contains("403"), "R2: {denied}");

    let unauthenticated = reqwest::Client::new()
        .post(format!(
            "http://{address}/v1/worker/resources/repositories/verify"
        ))
        .json(&serde_json::json!({
            "claim": claim,
            "identity": identity,
            "workspace_id": "workspace-repository",
            "repository_id": "repository-exact",
            "config_version": ConfigVersion::INITIAL,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        unauthenticated.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "R3"
    );

    let stale_verifier =
        HttpRepositoryBindingVerifier::new(
            WorkerUpstream::new(format!("http://{address}")).with_worker_identity(
                WorkerIdentity::new("worker-repository", "worker-repository-stale", 2),
            ),
        );
    let stale_identity = stale_verifier
        .verify(
            "workspace-repository",
            "repository-exact",
            ConfigVersion::INITIAL,
            Some(&claim),
        )
        .await
        .expect_err("R4 stale incarnation must be denied");
    assert!(stale_identity.to_string().contains("403"), "R4");

    let denied_service = Arc::new(
        WorkerRepositoryBindingService::new(
            Arc::new(ExactRepositoryRegistry { active: false }),
            dispatch.clone(),
            Arc::new(HeaderWorkerAuthenticator),
        )
        .with_worker_directory(directory),
    );
    let denied_address = support::serve(worker_repository_binding_router(denied_service)).await;
    let denied_verifier = HttpRepositoryBindingVerifier::new(
        WorkerUpstream::new(format!("http://{denied_address}"))
            .with_worker_identity(identity.clone()),
    );
    let inactive = denied_verifier
        .verify(
            "workspace-repository",
            "repository-exact",
            ConfigVersion::INITIAL,
            Some(&claim),
        )
        .await
        .expect_err("R6 inactive Repository must be denied");
    assert!(inactive.to_string().contains("403"), "R6");

    dispatch
        .settle(
            &claim.run_id,
            claim.epoch,
            awaken_run_ingress::DispatchOutcome::Done,
            &[],
        )
        .await
        .unwrap();
    let stale_claim = verifier
        .verify(
            "workspace-repository",
            "repository-exact",
            ConfigVersion::INITIAL,
            Some(&claim),
        )
        .await
        .expect_err("R5 stale claim must be rejected");
    assert!(stale_claim.to_string().contains("409"), "R5");
}

/// Repository transport decision table:
///
/// | Rule | common Worker/claim/manifest checks | deployment authorizer | Effect |
/// |---|---|---|---|
/// | T1 | pass | absent | return `Direct` for the self-hosted composition |
/// | T2 | pass | returns exact mediated transport | pass the exact claim expiry and return only its rewritten URL and short capability |
/// | T3 | pass | denies or is unavailable | reject; never return `Direct` as a fallback |
/// | T4 | claim is cancelled while authorizer awaits | returns transport | final locked recheck rejects it |
/// | T5 | pass | returns expired actual expiry | reject HTTP 403 before transport leaves authority |
/// | T6 | pass | expiry exceeds claim | reject HTTP 403 before transport leaves authority |
/// | T7 | pass | legacy expiry omitted | preserve existing one-shot host-operation wire |
///
/// State-machine coverage: `Claimed -> Authorizing -> Revalidated -> Mediated`
/// is the only Cloud success path. `Claimed -> CancelRequested` during
/// `Authorizing` transitions to `Rejected`; the authorizer result cannot revive
/// the claim. The exact claim expiry travels with the authorization so the
/// resulting capability cannot outlive `Claimed`.
///
/// T1 is covered by `repository_verification_is_scope_claim_manifest_and_incarnation_fenced`.
#[tokio::test]
async fn repository_transport_authorizer_is_exact_and_has_no_direct_fallback() {
    let (directory, identity) = support::ready_worker("worker-repository-gateway").await;
    let dispatch = Arc::new(MemoryDispatchStore::new());
    let claim = claimed_dispatch(&dispatch, &identity.lease_owner()).await;
    let authorizer = Arc::new(GatewayAuthorizer::default());
    let service = Arc::new(
        WorkerRepositoryBindingService::new(
            Arc::new(ExactRepositoryRegistry { active: true }),
            dispatch.clone(),
            Arc::new(HeaderWorkerAuthenticator),
        )
        .with_worker_directory(directory.clone())
        .with_transport_authorizer(authorizer.clone()),
    );
    let address = support::serve(worker_repository_binding_router(service)).await;
    let verifier = HttpRepositoryBindingVerifier::new(
        WorkerUpstream::new(format!("http://{address}")).with_worker_identity(identity.clone()),
    );

    let transport = verifier
        .verify(
            "workspace-repository",
            "repository-exact",
            ConfigVersion::INITIAL,
            Some(&claim),
        )
        .await
        .expect("T2 exact Gateway transport");
    assert!(
        matches!(
            transport,
            awaken_resource_contract::RepositoryTransport::GatewayMediated { ref remote_url, .. }
                if remote_url == "https://gateway.invalid/git/repository-exact"
        ),
        "T2"
    );
    {
        let calls = authorizer.0.lock().unwrap();
        assert_eq!(calls.len(), 1, "T2");
        assert_eq!(calls[0].worker, identity, "T2");
        let RepositoryTransportAuthority::Run {
            claim: authorized_claim,
            claim_expires_ms,
        } = &calls[0].authority
        else {
            panic!("T2 Run authority");
        };
        assert_eq!(authorized_claim, &claim, "T2");
        assert!(*claim_expires_ms >= support::unix_now_ms(), "T2");
        assert_eq!(calls[0].workspace_id, "workspace-repository", "T2");
    }

    let denied = Arc::new(
        WorkerRepositoryBindingService::new(
            Arc::new(ExactRepositoryRegistry { active: true }),
            dispatch,
            Arc::new(HeaderWorkerAuthenticator),
        )
        .with_worker_directory(directory)
        .with_transport_authorizer(Arc::new(DeniedAuthorizer)),
    );
    let denied_address = support::serve(worker_repository_binding_router(denied)).await;
    let denied_verifier = HttpRepositoryBindingVerifier::new(
        WorkerUpstream::new(format!("http://{denied_address}")).with_worker_identity(identity),
    );
    let error = denied_verifier
        .verify(
            "workspace-repository",
            "repository-exact",
            ConfigVersion::INITIAL,
            Some(&claim),
        )
        .await
        .expect_err("T3 denial must not fall back to direct transport");
    assert!(error.to_string().contains("403"), "T3: {error}");

    let (directory, identity) = support::ready_worker("worker-repository-cancel").await;
    let dispatch = Arc::new(MemoryDispatchStore::new());
    let claim = claimed_dispatch(&dispatch, &identity.lease_owner()).await;
    let cancelling = Arc::new(
        WorkerRepositoryBindingService::new(
            Arc::new(ExactRepositoryRegistry { active: true }),
            dispatch.clone(),
            Arc::new(HeaderWorkerAuthenticator),
        )
        .with_worker_directory(directory)
        .with_transport_authorizer(Arc::new(CancellingAuthorizer {
            dispatch,
            run_id: claim.run_id.clone(),
        })),
    );
    let address = support::serve(worker_repository_binding_router(cancelling)).await;
    let verifier = HttpRepositoryBindingVerifier::new(
        WorkerUpstream::new(format!("http://{address}")).with_worker_identity(identity),
    );
    let error = verifier
        .verify(
            "workspace-repository",
            "repository-exact",
            ConfigVersion::INITIAL,
            Some(&claim),
        )
        .await
        .expect_err("T4 cancellation during authorization must be rejected");
    assert!(error.to_string().contains("409"), "T4: {error}");

    for (rule, expiry, accepted) in [
        ("T5", Some(1), false),
        ("T6", Some(u64::MAX), false),
        ("T7", None, true),
    ] {
        let (directory, identity) =
            support::ready_worker(&format!("worker-repository-expiry-{rule}")).await;
        let dispatch = Arc::new(MemoryDispatchStore::new());
        let claim = claimed_dispatch(&dispatch, &identity.lease_owner()).await;
        let service = Arc::new(
            WorkerRepositoryBindingService::new(
                Arc::new(ExactRepositoryRegistry { active: true }),
                dispatch,
                Arc::new(HeaderWorkerAuthenticator),
            )
            .with_worker_directory(directory)
            .with_transport_authorizer(Arc::new(FixedExpiryAuthorizer(expiry))),
        );
        let address = support::serve(worker_repository_binding_router(service)).await;
        let verifier = HttpRepositoryBindingVerifier::new(
            WorkerUpstream::new(format!("http://{address}")).with_worker_identity(identity),
        );
        let result = verifier
            .verify(
                "workspace-repository",
                "repository-exact",
                ConfigVersion::INITIAL,
                Some(&claim),
            )
            .await;
        if accepted {
            assert!(result.is_ok(), "{rule}: {result:?}");
        } else {
            let error = result.expect_err(rule);
            assert!(error.to_string().contains("403"), "{rule}: {error}");
        }
    }
}

/// Terminal Repository authority decision table:
///
/// | Rule | registered Worker | realization lease | canonical command | deployment authorizer | Effect |
/// |---|---|---|---|---|---|
/// | P1 | current | exact/live | exact, catalog retired | absent | Direct from the frozen command; no mutable catalog re-read |
/// | P2 | current | exact/live | exact | Gateway | exact terminal authority reaches authorizer |
/// | P3 | stale incarnation | exact | exact | any | deny before Control/authorizer |
/// | P4 | current | stale | exact | any | reject the non-current generation |
/// | P5 | current | exact | changed | any | reject before Repository transport |
/// | P6 | current | exact | exact command, foreign request Workspace | any | deny before authorizer; Session owner is authoritative |
/// | P7 | current | exact | exact | no Session Control | unavailable, never RunClaim fallback |
/// | P8 | current | exact/live | exact | Gateway expiry expired | HTTP 403; no stale capability leaves the terminal boundary |
/// | P9 | current | asserted expired, same-generation root live | exact | operation-scoped Gateway expiry 25s beyond the 20s root lease | admit one slow publication capability; the issuer, not the heartbeat, owns its one-shot expiry |
/// | P10 | current | exact/live | exact | Gateway expiry omitted | HTTP 403; the new terminal entry requires issuer-owned live expiry |
/// | P11 | current | asserted expired, current root expired | exact | any | 409 from Control; no authorizer |
/// | P12 | current | asserted expired, root renews B -> C during authorizer | exact | operation-scoped Gateway expiry | admit after stable Workspace/command and monotonic same-generation recheck; do not require whole-projection equality |
#[tokio::test]
async fn terminal_repository_transport_is_worker_lease_and_command_fenced() {
    type PublicationFence = (
        awaken_session_contract::SessionRepositoryPublicationCommand,
        awaken_session_contract::SessionRealizationLease,
    );

    let (directory, identity) = support::ready_worker("worker-repository-publication").await;
    let command = terminal_publication_command("session-publication");
    let lease = awaken_session_contract::SessionRealizationLease {
        owner: identity.worker_id.clone(),
        runtime_incarnation: identity.lease_owner(),
        epoch: 7,
        expires_at_unix_ms: support::unix_now_ms().saturating_add(20_000),
    };
    let control = Arc::new(ExactTerminalPublicationControl {
        workspace_id: "workspace-repository".into(),
        command: command.clone(),
        lease: Arc::new(Mutex::new(lease.clone())),
    });
    let direct = Arc::new(
        WorkerRepositoryBindingService::new(
            Arc::new(ExactRepositoryRegistry { active: false }),
            Arc::new(MemoryDispatchStore::new()),
            Arc::new(HeaderWorkerAuthenticator),
        )
        .with_worker_directory(directory.clone())
        .with_session_control(control.clone()),
    );
    let address = support::serve(worker_repository_binding_router(direct)).await;
    let verifier = HttpRepositoryBindingVerifier::new(
        WorkerUpstream::new(format!("http://{address}")).with_worker_identity(identity.clone()),
    );
    let fence = (command.clone(), lease.clone());
    let transport =
        <HttpRepositoryBindingVerifier as awaken_resource_contract::RepositoryBindingVerifier<
            PublicationFence,
        >>::verify(
            &verifier,
            "workspace-repository",
            "repository-exact",
            ConfigVersion::INITIAL,
            Some(&fence),
        )
        .await
        .expect("P1 exact terminal Direct authority");
    assert!(
        matches!(
            transport,
            awaken_resource_contract::RepositoryTransport::Direct
        ),
        "P1"
    );

    let authorizer = Arc::new(GatewayAuthorizer::default());
    let gateway = Arc::new(
        WorkerRepositoryBindingService::new(
            Arc::new(ExactRepositoryRegistry { active: true }),
            Arc::new(MemoryDispatchStore::new()),
            Arc::new(HeaderWorkerAuthenticator),
        )
        .with_worker_directory(directory.clone())
        .with_session_control(control.clone())
        .with_transport_authorizer(authorizer.clone()),
    );
    let address = support::serve(worker_repository_binding_router(gateway)).await;
    let gateway_verifier = HttpRepositoryBindingVerifier::new(
        WorkerUpstream::new(format!("http://{address}")).with_worker_identity(identity.clone()),
    );
    <HttpRepositoryBindingVerifier as awaken_resource_contract::RepositoryBindingVerifier<
        PublicationFence,
    >>::verify(
        &gateway_verifier,
        "workspace-repository",
        "repository-exact",
        ConfigVersion::INITIAL,
        Some(&fence),
    )
    .await
    .expect("P2 Gateway terminal authority");
    {
        let calls = authorizer.0.lock().unwrap();
        assert_eq!(calls.len(), 1, "P2");
        assert!(
            matches!(
                &calls[0].authority,
                RepositoryTransportAuthority::TerminalPublication {
                    command: authorized,
                    lease: authorized_lease,
                } if authorized.as_ref() == &command && authorized_lease == &lease
            ),
            "P2"
        );
    }

    let slow_publication_expiry = lease.expires_at_unix_ms.saturating_add(25_000);
    for (rule, expiry, accepted) in [
        ("P8", Some(1), false),
        ("P9", Some(slow_publication_expiry), true),
        ("P10", None, false),
    ] {
        let expiry_service = Arc::new(
            WorkerRepositoryBindingService::new(
                Arc::new(ExactRepositoryRegistry { active: true }),
                Arc::new(MemoryDispatchStore::new()),
                Arc::new(HeaderWorkerAuthenticator),
            )
            .with_worker_directory(directory.clone())
            .with_session_control(control.clone())
            .with_transport_authorizer(Arc::new(FixedExpiryAuthorizer(expiry))),
        );
        let expiry_address = support::serve(worker_repository_binding_router(expiry_service)).await;
        let expiry_verifier = HttpRepositoryBindingVerifier::new(
            WorkerUpstream::new(format!("http://{expiry_address}"))
                .with_worker_identity(identity.clone()),
        );
        let result = <HttpRepositoryBindingVerifier as awaken_resource_contract::RepositoryBindingVerifier<
            PublicationFence,
        >>::verify(
            &expiry_verifier,
            "workspace-repository",
            "repository-exact",
            ConfigVersion::INITIAL,
            Some(&fence),
        )
        .await;
        if accepted {
            assert!(result.is_ok(), "{rule}: {result:?}");
        } else {
            let error = result.expect_err(rule);
            assert!(error.to_string().contains("403"), "{rule}: {error}");
        }
    }

    let stale = HttpRepositoryBindingVerifier::new(
        WorkerUpstream::new(format!("http://{address}")).with_worker_identity(WorkerIdentity::new(
            identity.worker_id.clone(),
            "stale-publication-incarnation",
            identity.generation,
        )),
    );
    let error =
        <HttpRepositoryBindingVerifier as awaken_resource_contract::RepositoryBindingVerifier<
            PublicationFence,
        >>::verify(
            &stale,
            "workspace-repository",
            "repository-exact",
            ConfigVersion::INITIAL,
            Some(&fence),
        )
        .await
        .expect_err("P3 stale Worker");
    assert!(error.to_string().contains("403"), "P3: {error}");

    let mut stale_lease = lease.clone();
    stale_lease.epoch += 1;
    let stale_fence = (command.clone(), stale_lease);
    let error =
        <HttpRepositoryBindingVerifier as awaken_resource_contract::RepositoryBindingVerifier<
            PublicationFence,
        >>::verify(
            &gateway_verifier,
            "workspace-repository",
            "repository-exact",
            ConfigVersion::INITIAL,
            Some(&stale_fence),
        )
        .await
        .expect_err("P4 stale lease");
    assert!(error.to_string().contains("409"), "P4: {error}");

    let mut expired_assertion = lease.clone();
    expired_assertion.expires_at_unix_ms = 1;
    let expired_assertion_fence = (command.clone(), expired_assertion.clone());
    let slow_publication_service = Arc::new(
        WorkerRepositoryBindingService::new(
            Arc::new(ExactRepositoryRegistry { active: true }),
            Arc::new(MemoryDispatchStore::new()),
            Arc::new(HeaderWorkerAuthenticator),
        )
        .with_worker_directory(directory.clone())
        .with_session_control(control.clone())
        .with_transport_authorizer(Arc::new(FixedExpiryAuthorizer(Some(
            slow_publication_expiry,
        )))),
    );
    let slow_publication_address =
        support::serve(worker_repository_binding_router(slow_publication_service)).await;
    let slow_publication_verifier = HttpRepositoryBindingVerifier::new(
        WorkerUpstream::new(format!("http://{slow_publication_address}"))
            .with_worker_identity(identity.clone()),
    );
    let transport =
        <HttpRepositoryBindingVerifier as awaken_resource_contract::RepositoryBindingVerifier<
            PublicationFence,
        >>::verify(
            &slow_publication_verifier,
            "workspace-repository",
            "repository-exact",
            ConfigVersion::INITIAL,
            Some(&expired_assertion_fence),
        )
        .await
        .expect("P9 expired assertion is authorized by the live same-generation root");
    assert!(
        matches!(
            transport,
            awaken_resource_contract::RepositoryTransport::GatewayMediated {
                expires_at_unix_ms: Some(expiry),
                ..
            } if expiry.unix_ms() > lease.expires_at_unix_ms
        ),
        "P9 operation capability covers the simulated >20s publication window"
    );
    assert!(
        slow_publication_expiry.saturating_sub(lease.expires_at_unix_ms) > 20_000,
        "P9 simulates a publication that outlives the 20s root heartbeat"
    );

    let expired_control = Arc::new(ExactTerminalPublicationControl {
        workspace_id: "workspace-repository".into(),
        command: command.clone(),
        lease: Arc::new(Mutex::new(expired_assertion.clone())),
    });
    let rejected_authorizer = Arc::new(GatewayAuthorizer::default());
    let expired_service = Arc::new(
        WorkerRepositoryBindingService::new(
            Arc::new(ExactRepositoryRegistry { active: true }),
            Arc::new(MemoryDispatchStore::new()),
            Arc::new(HeaderWorkerAuthenticator),
        )
        .with_worker_directory(directory.clone())
        .with_session_control(expired_control)
        .with_transport_authorizer(rejected_authorizer.clone()),
    );
    let expired_address = support::serve(worker_repository_binding_router(expired_service)).await;
    let expired_verifier = HttpRepositoryBindingVerifier::new(
        WorkerUpstream::new(format!("http://{expired_address}"))
            .with_worker_identity(identity.clone()),
    );
    let error =
        <HttpRepositoryBindingVerifier as awaken_resource_contract::RepositoryBindingVerifier<
            PublicationFence,
        >>::verify(
            &expired_verifier,
            "workspace-repository",
            "repository-exact",
            ConfigVersion::INITIAL,
            Some(&expired_assertion_fence),
        )
        .await
        .expect_err("P11 expired current root");
    assert!(error.to_string().contains("409"), "P11: {error}");
    assert_eq!(rejected_authorizer.0.lock().unwrap().len(), 0, "P11");

    let renewal = awaken_session_contract::SessionRealizationLease {
        expires_at_unix_ms: lease.expires_at_unix_ms.saturating_add(10_000),
        ..lease.clone()
    };
    let renewing_current = Arc::new(Mutex::new(lease.clone()));
    let renewal_capability_expiry = renewal.expires_at_unix_ms.saturating_add(25_000);
    let renewing_control = Arc::new(ExactTerminalPublicationControl {
        workspace_id: "workspace-repository".into(),
        command: command.clone(),
        lease: renewing_current.clone(),
    });
    let renewing_service = Arc::new(
        WorkerRepositoryBindingService::new(
            Arc::new(ExactRepositoryRegistry { active: true }),
            Arc::new(MemoryDispatchStore::new()),
            Arc::new(HeaderWorkerAuthenticator),
        )
        .with_worker_directory(directory.clone())
        .with_session_control(renewing_control)
        .with_transport_authorizer(Arc::new(RenewingTerminalAuthorizer {
            current_lease: renewing_current.clone(),
            renewal: renewal.clone(),
            capability_expiry_unix_ms: renewal_capability_expiry,
        })),
    );
    let renewing_address = support::serve(worker_repository_binding_router(renewing_service)).await;
    let renewing_verifier = HttpRepositoryBindingVerifier::new(
        WorkerUpstream::new(format!("http://{renewing_address}"))
            .with_worker_identity(identity.clone()),
    );
    let transport =
        <HttpRepositoryBindingVerifier as awaken_resource_contract::RepositoryBindingVerifier<
            PublicationFence,
        >>::verify(
            &renewing_verifier,
            "workspace-repository",
            "repository-exact",
            ConfigVersion::INITIAL,
            Some(&expired_assertion_fence),
        )
        .await
        .expect("P12 same-generation renewal during authorization");
    assert_eq!(*renewing_current.lock().unwrap(), renewal, "P12");
    assert!(
        matches!(
            transport,
            awaken_resource_contract::RepositoryTransport::GatewayMediated {
                expires_at_unix_ms: Some(expiry),
                ..
            } if expiry.unix_ms() == renewal_capability_expiry
        ),
        "P12"
    );

    let mut changed = command.clone();
    changed.effect_id.push_str("-changed");
    let changed_fence = (changed, lease.clone());
    let error =
        <HttpRepositoryBindingVerifier as awaken_resource_contract::RepositoryBindingVerifier<
            PublicationFence,
        >>::verify(
            &gateway_verifier,
            "workspace-repository",
            "repository-exact",
            ConfigVersion::INITIAL,
            Some(&changed_fence),
        )
        .await
        .expect_err("P5 changed command");
    assert!(error.to_string().contains("409"), "P5: {error}");

    let error =
        <HttpRepositoryBindingVerifier as awaken_resource_contract::RepositoryBindingVerifier<
            PublicationFence,
        >>::verify(
            &gateway_verifier,
            "workspace-forged-by-worker",
            "repository-exact",
            ConfigVersion::INITIAL,
            Some(&fence),
        )
        .await
        .expect_err("P6 Worker-selected Workspace must not replace the Session owner");
    assert!(error.to_string().contains("403"), "P6: {error}");
    assert_eq!(
        authorizer.0.lock().unwrap().len(),
        1,
        "P6 before authorizer"
    );

    let unavailable = Arc::new(
        WorkerRepositoryBindingService::new(
            Arc::new(ExactRepositoryRegistry { active: true }),
            Arc::new(MemoryDispatchStore::new()),
            Arc::new(HeaderWorkerAuthenticator),
        )
        .with_worker_directory(directory),
    );
    let address = support::serve(worker_repository_binding_router(unavailable)).await;
    let unavailable_verifier = HttpRepositoryBindingVerifier::new(
        WorkerUpstream::new(format!("http://{address}")).with_worker_identity(identity),
    );
    let error =
        <HttpRepositoryBindingVerifier as awaken_resource_contract::RepositoryBindingVerifier<
            PublicationFence,
        >>::verify(
            &unavailable_verifier,
            "workspace-repository",
            "repository-exact",
            ConfigVersion::INITIAL,
            Some(&fence),
        )
        .await
        .expect_err("P7 missing Control");
    assert!(error.to_string().contains("503"), "P7: {error}");
}

/// Cause/effect rationale: a remote verifier without the exact dispatch claim
/// has no authority to contact the Repository boundary, so C1 -> E1 fails
/// locally and cannot become an unfenced compatibility path.
#[tokio::test]
async fn remote_repository_verifier_requires_claim() {
    let verifier = HttpRepositoryBindingVerifier::new(WorkerUpstream::new("http://127.0.0.1:1"));
    let error = verifier
        .verify(
            "workspace-repository",
            "repository-exact",
            ConfigVersion::INITIAL,
            None::<&RunClaim>,
        )
        .await
        .expect_err("claim is mandatory");
    assert!(error.to_string().contains("requires a dispatch claim"));

    let unused = RunClaim {
        run_id: RunId("run-unused".into()),
        owner: "worker-unused".into(),
        epoch: 1,
    };
    let empty = verifier
        .verify(
            "",
            "repository-exact",
            ConfigVersion::INITIAL,
            Some(&unused),
        )
        .await
        .expect_err("empty Workspace is rejected locally");
    assert!(empty.to_string().contains("must not be empty"));
}
