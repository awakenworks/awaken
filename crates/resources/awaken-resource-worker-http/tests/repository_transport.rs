//! Repository binding boundary tests over real HTTP.

use awaken_run_ingress_testkit::worker_http as support;

use std::sync::Arc;

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_resource_contract::RepositoryBindingVerifier as _;
use awaken_resource_contract::{ConfigVersion, ResourceBindingValidator, ResourceCatalogError};
use awaken_resource_worker_http::HttpRepositoryBindingVerifier;
use awaken_resource_worker_http::{
    WorkerRepositoryBindingService, worker_repository_binding_router,
};
use awaken_run_ingress::{
    DispatchQueue as _, MemoryDispatchStore, RunClaim, RunDispatch, WorkerIdentity,
};
use awaken_worker_transport_security::{HeaderWorkerAuthenticator, WorkerUpstream};

struct ExactRepositoryCatalog {
    active: bool,
}

impl ResourceBindingValidator for ExactRepositoryCatalog {
    fn validate_memory_binding(
        &self,
        _workspace_id: &str,
        _id: &str,
        _version: ConfigVersion,
    ) -> Result<(), ResourceCatalogError> {
        Err(ResourceCatalogError::NotFound("memory".into()))
    }

    fn validate_repository_binding(
        &self,
        workspace_id: &str,
        id: &str,
        version: ConfigVersion,
    ) -> Result<(), ResourceCatalogError> {
        if self.active
            && workspace_id == "workspace-repository"
            && id == "repository-exact"
            && version == ConfigVersion::INITIAL
        {
            Ok(())
        } else {
            Err(ResourceCatalogError::NotFound(id.into()))
        }
    }
}

fn resources() -> awaken_session_contract::ResolvedSessionResources {
    awaken_session_contract::ResolvedSessionResources {
        inputs: vec![awaken_session_contract::ResolvedInput {
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
        skills: Some(Vec::new()),
    }
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
            Arc::new(ExactRepositoryCatalog { active: true }),
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
            Arc::new(ExactRepositoryCatalog { active: false }),
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
            None,
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
