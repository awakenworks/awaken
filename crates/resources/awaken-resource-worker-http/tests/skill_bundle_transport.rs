//! Custom-Skill Resource Coordinator boundary tests over real HTTP.

use awaken_run_ingress_testkit::worker_http as support;

use std::sync::Arc;

use awaken_agent_contract::AgentSkillKind;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_resource_application::StoreSkillBundleSource;
use awaken_resource_worker_http::{
    HttpSkillBundleSource, WorkerSkillBundleService, worker_skill_bundle_router,
};
use awaken_run_ingress::{
    DispatchQueue as _, MemoryDispatchStore, RunClaim, RunDispatch, WorkerIdentity,
};
use awaken_session_contract::{
    ResolvedSessionResources, ResolvedSkillBinding, SkillBundleSource as _,
};
use awaken_skill_store::{
    InMemorySkillStore, SkillBundleFile, SkillDefinition, SkillStore as _, SkillVersion,
    bundle_sha256,
};
use awaken_worker_transport_security::{HeaderWorkerAuthenticator, WorkerUpstream};

fn version(skill_id: &str, ordinal: u64, body: &[u8]) -> SkillVersion {
    let files = vec![SkillBundleFile {
        path: "SKILL.md".into(),
        content: body.to_vec(),
        executable: false,
    }];
    SkillVersion {
        id: format!("skver-{skill_id}-{ordinal}").into(),
        skill_id: skill_id.into(),
        version: ordinal,
        name: skill_id.into(),
        description: "exact test bundle".into(),
        directory: format!("/skills/{skill_id}"),
        bundle_sha256: bundle_sha256(&files),
        files,
        created_unix_nanos: ordinal,
    }
}

fn binding(version: &SkillVersion) -> ResolvedSkillBinding {
    ResolvedSkillBinding {
        kind: AgentSkillKind::Custom,
        skill_id: version.skill_id.to_string(),
        version: version.version,
        bundle_sha256: version.bundle_sha256.clone(),
    }
}

async fn claimed_dispatch(
    dispatch: &Arc<MemoryDispatchStore>,
    frozen: &ResolvedSkillBinding,
    owner: &str,
) -> RunClaim {
    let resources = ResolvedSessionResources {
        inputs: Vec::new(),
        skills: Some(vec![frozen.clone()]),
    };
    let request = RunDispatch::new(support::activation("skill-bundle"))
        .with_execution_scope(awaken_tenancy::ExecutionScopeRef(
            awaken_tenancy::ScopeId::from("workspace-skill"),
        ))
        .with_session_resources(awaken_run_ingress::SessionResourceEnvelope::new(
            "workspace-skill",
            serde_json::to_string(&resources).unwrap(),
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
/// | Rule | Worker auth | live exact claim | frozen custom binding | returned bundle | Effect |
/// |---|---|---|---|---|---|
/// | S1 | valid incarnation | yes | exact Workspace/id/version/hash | exact | return immutable bundle |
/// | S2 | valid incarnation | yes | another binding | any | deny before Skill source |
/// | S3 | missing | any | any | any | HTTP 401 before claim/store |
/// | S4 | stale incarnation | yes | exact | exact | deny before Skill source |
/// | S5 | valid incarnation | stale epoch | exact | any | reject without bundle |
/// | S6 | valid | yes | exact | substituted bytes | Worker rejects bundle |
#[tokio::test]
async fn exact_skill_bundle_is_scope_claim_and_incarnation_fenced_and_digest_verified() {
    let stored = version("review", 1, b"---\ndescription: exact\n---\nREVIEW");
    let frozen = binding(&stored);
    let store = Arc::new(InMemorySkillStore::new());
    store
        .create(
            SkillDefinition {
                id: "review".into(),
                workspace_id: "workspace-skill".into(),
                display_title: None,
                latest_version: 1,
                last_version: 1,
                timestamps: Default::default(),
            },
            stored.clone(),
        )
        .await
        .unwrap();
    let (directory, identity) = support::ready_worker("worker-skill").await;
    let dispatch = Arc::new(MemoryDispatchStore::new());
    let claim = claimed_dispatch(&dispatch, &frozen, &identity.lease_owner()).await;
    let service = Arc::new(
        WorkerSkillBundleService::new(
            Arc::new(StoreSkillBundleSource::new(store)),
            dispatch.clone(),
            Arc::new(HeaderWorkerAuthenticator),
        )
        .with_worker_directory(directory),
    );
    let address = support::serve(worker_skill_bundle_router(service)).await;
    let source = HttpSkillBundleSource::new(
        WorkerUpstream::new(format!("http://{address}")).with_worker_identity(identity.clone()),
    );

    let exact = source
        .load("workspace-skill", &frozen, Some(&claim))
        .await
        .expect("S1 exact Skill read")
        .expect("S1 existing Skill");
    assert_eq!(exact, stored, "S1");

    let other = binding(&version("other", 1, b"OTHER"));
    let denied = source
        .load("workspace-skill", &other, Some(&claim))
        .await
        .expect_err("S2 non-frozen Skill must be denied");
    assert!(denied.to_string().contains("403"), "S2: {denied}");

    let unauthenticated = reqwest::Client::new()
        .post(format!(
            "http://{address}/v1/worker/resources/skills/bundle"
        ))
        .json(&serde_json::json!({
            "claim": claim,
            "identity": identity,
            "workspace_id": "workspace-skill",
            "binding": frozen
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        unauthenticated.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "S3"
    );

    let stale_source =
        HttpSkillBundleSource::new(
            WorkerUpstream::new(format!("http://{address}"))
                .with_worker_identity(WorkerIdentity::new("worker-skill", "worker-skill-stale", 2)),
        );
    let stale_identity = stale_source
        .load("workspace-skill", &frozen, Some(&claim))
        .await
        .expect_err("S4 stale incarnation must be denied");
    assert!(stale_identity.to_string().contains("403"), "S4");

    dispatch
        .settle(
            &claim.run_id,
            claim.epoch,
            awaken_run_ingress::DispatchOutcome::Done,
            &[],
        )
        .await
        .unwrap();
    let stale_claim = source
        .load("workspace-skill", &frozen, Some(&claim))
        .await
        .expect_err("S5 stale claim must be rejected");
    assert!(stale_claim.to_string().contains("409"), "S5");

    let substituted = version("review", 1, b"SUBSTITUTED");
    let substituted_router = axum::Router::new().route(
        "/v1/worker/resources/skills/bundle",
        axum::routing::post(move || {
            let substituted = substituted.clone();
            async move { axum::Json(substituted) }
        }),
    );
    let substituted_address = support::serve(substituted_router).await;
    let substituted_source = HttpSkillBundleSource::new(
        WorkerUpstream::new(format!("http://{substituted_address}")).with_worker_id("worker-skill"),
    );
    assert!(
        substituted_source
            .load("workspace-skill", &frozen, Some(&claim))
            .await
            .is_err(),
        "S6"
    );
}

/// Cause/effect rationale: without an exact dispatch claim a remote source has
/// no authority to contact the Skill data plane, so C1 -> E1 fails locally and
/// cannot become an unfenced compatibility path. Built-in Skills are rejected
/// by this port because the runtime binary is their sole source of truth.
#[tokio::test]
async fn remote_skill_source_requires_claim_and_custom_kind() {
    let source = HttpSkillBundleSource::new(WorkerUpstream::new("http://127.0.0.1:1"));
    let custom = binding(&version("review", 1, b"REVIEW"));
    let missing_claim = source
        .load("workspace-skill", &custom, None)
        .await
        .expect_err("claim is mandatory");
    assert!(
        missing_claim
            .to_string()
            .contains("requires a dispatch claim")
    );

    let mut built_in = custom;
    built_in.kind = AgentSkillKind::Anthropic;
    let claim = RunClaim {
        run_id: RunId("run-unused".into()),
        owner: "worker-unused".into(),
        epoch: 1,
    };
    let wrong_kind = source
        .load("workspace-skill", &built_in, Some(&claim))
        .await
        .expect_err("built-in Skill stays runtime-owned");
    assert!(wrong_kind.to_string().contains("custom Skill"));
}
