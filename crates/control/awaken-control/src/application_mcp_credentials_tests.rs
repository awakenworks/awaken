use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use awaken_credential_contract::CredentialSourceId;
use awaken_credential_contract::{
    ManagedCredentialAdoptionError, ManagedCredentialAdoptionProgress,
    ManagedCredentialRolloutTarget,
};
use awaken_credential_vault::repo::{
    CredentialRepo, InMemoryCredentialRepo, ManagedCredentialRepository, ManagedCredentialRollout,
};
use awaken_credential_vault::{InMemorySecretStore, SecretStore};
use awaken_protocol_managed::VaultState;
use awaken_runtime_contract::runs::scope::WorkspaceScope;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use serde_json::{Value, json};
use tower::ServiceExt as _;

use super::application_mcp_credentials_router;

#[derive(Default)]
struct AdoptionTarget {
    fail: AtomicBool,
    pending: AtomicBool,
    events: std::sync::Mutex<Vec<ManagedCredentialRollout>>,
}

#[async_trait::async_trait]
impl ManagedCredentialRolloutTarget for AdoptionTarget {
    async fn rollout(
        &self,
        event: &ManagedCredentialRollout,
    ) -> Result<ManagedCredentialAdoptionProgress, ManagedCredentialAdoptionError> {
        self.events.lock().unwrap().push(event.clone());
        if self.fail.load(Ordering::SeqCst) {
            return Err(ManagedCredentialAdoptionError::Unavailable(
                awaken_credential_contract::ManagedCredentialAdoptionUnavailable::Transport,
            ));
        }
        if self.pending.load(Ordering::SeqCst) {
            return Ok(ManagedCredentialAdoptionProgress::Pending);
        }
        Ok(ManagedCredentialAdoptionProgress::Converged)
    }
}

async fn call(
    app: &axum::Router,
    workspace_id: &str,
    idempotency_key: Option<&str>,
    body: Value,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/v1/config/application-mcp-credentials")
        .header("content-type", "application/json");
    if let Some(key) = idempotency_key {
        builder = builder.header("idempotency-key", key);
    }
    let mut request = builder
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    request
        .extensions_mut()
        .insert(WorkspaceScope(workspace_id.to_owned()));
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn application_bearer_replay_revalidates_acknowledged_delivery_without_rotation() {
    // Cause/effect graph: C1 a generation-2 event was exactly acknowledged;
    // C2 its target later reports Pending, unavailable, or Converged; C3 the
    // exact material command replays; C4 generation 3 supersedes it. Effects:
    // E1 retain the exact acknowledged event for bounded primary-key reads;
    // E2 C2=Pending/unavailable re-arms that event, never returns false usable
    // access; E3 Converged exactly acknowledges without a source rotation;
    // E4 retain only the newer acknowledgement for the same actual source.
    // Rules: A1 C1 -> E1; A2 C1+C2=P+C3 -> E2; A3 C2=unavailable+C3 -> E2;
    // A4 C2=C+C3 -> E3; A5 C4 -> E4. This is the real HTTP/Vault/WAL path,
    // with only target availability controlled; no model or bearer echo.
    let credentials = Arc::new(InMemoryCredentialRepo::new());
    let state = Arc::new(VaultState::new(
        Arc::new(InMemorySecretStore::new()),
        credentials.clone(),
        awaken_time::system_clock(),
        None,
    ));
    let target = Arc::new(AdoptionTarget::default());
    state.application().set_rollout_target(target.clone());
    let app = application_mcp_credentials_router(state);
    let body = |generation| {
        json!({
            "application_authority_id":"retained-session",
            "mcp_server_url":"https://flow.example.test/mcp",
            "credential_generation":generation,
            "token":format!("synthetic-bearer-{generation}") // awaken-allow(secret):
        })
    };
    for generation in [1, 2] {
        let (status, receipt) = call(
            &app,
            "workspace-a",
            Some(&format!("bearer-{generation}")),
            body(generation),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(receipt["adoption"], "converged");
    }
    let event = target.events.lock().unwrap()[0].clone();
    assert_eq!(
        credentials.managed_rollout(&event.id).await.unwrap(),
        Some(event.clone()),
        "A1 exact receipt is retained after acknowledgement"
    );
    assert!(
        credentials
            .pending_managed_rollouts()
            .await
            .unwrap()
            .is_empty(),
        "A1 acknowledged is not background-pending"
    );
    for (rule, pending, failed) in [("A2", true, false), ("A3", false, true)] {
        target.pending.store(pending, Ordering::SeqCst);
        target.fail.store(failed, Ordering::SeqCst);
        let (status, receipt) = call(&app, "workspace-a", Some("bearer-2"), body(2)).await;
        assert_eq!(status, StatusCode::OK, "{rule}");
        assert_eq!(receipt["revision"], 2, "{rule} no rotation");
        assert_eq!(
            receipt["adoption"], "pending",
            "{rule} historical ack is not current adoption"
        );
        assert_eq!(
            credentials.pending_managed_rollouts().await.unwrap(),
            vec![event.clone()],
            "{rule} same event rearmed"
        );
    }
    target.pending.store(false, Ordering::SeqCst);
    target.fail.store(false, Ordering::SeqCst);
    let (_, recovered) = call(&app, "workspace-a", Some("bearer-2"), body(2)).await;
    assert_eq!(recovered["revision"], 2, "A4");
    assert_eq!(recovered["adoption"], "converged", "A4");
    assert!(
        credentials
            .pending_managed_rollouts()
            .await
            .unwrap()
            .is_empty(),
        "A4"
    );
    assert_eq!(
        credentials.managed_rollout(&event.id).await.unwrap(),
        Some(event.clone()),
        "A4 stable receipt"
    );
    let (_, newer) = call(&app, "workspace-a", Some("bearer-3"), body(3)).await;
    assert_eq!(newer["revision"], 3, "A5");
    assert_eq!(newer["adoption"], "converged", "A5");
    assert!(
        credentials
            .managed_rollout(&event.id)
            .await
            .unwrap()
            .is_none(),
        "A5 old acknowledged receipt pruned"
    );
    let latest = target.events.lock().unwrap().last().unwrap().clone();
    assert_eq!(
        credentials.managed_rollout(&latest.id).await.unwrap(),
        Some(latest),
        "A5 current exact receipt retained"
    );
}

/// Hosted application command cause/effect graph: C1 trusted Workspace is
/// stamped, C2 authority/target are valid, C3 replay key exists, C4 positive
/// generation and key match the current material, C5 generation is newer, C6
/// generation is zero/stale/same-but-conflicting, C7 the exact rollout has no
/// target, a pending target, a failed target, or a converged target. Effects:
/// E1 one stable Vault/source at revision 1, E2 exact replay, E3 reject before
/// material/state writes, E4 one next revision plus a durable rollout, E5
/// secret-free output, E6 exact `pending` or `converged` adoption without a
/// second mutation. Constraints: only the exact current
/// `(generation,key,payload)` replays; generation increases monotonically; only
/// exact target convergence removes the durable rollout.
///
/// | rule | command | exact rollout target | effect |
/// |---|---|---|---|
/// | M0 | generation zero | - | E3/400; no source or secret |
/// | M1 | create generation 1 | no predecessor event | E1 + E5 + converged |
/// | M2 | exact generation/key/payload replay | no predecessor event | E2 + E5 + converged |
/// | M3 | same generation, different key or payload | - | E3/409 |
/// | M4 | generation 2 rotation | absent | E4 + E5 + pending |
/// | M5 | exact generation 2 replay | pending or failed | E2 + E5 + pending |
/// | M6 | exact generation 2 replay | converged | E2 + E5 + converged; exact ack |
/// | M7 | generation 3 rotation | converged | E4 + E5 + converged; exact ack |
/// | M8 | delayed generation 1 replay | - | E3/409; generation 3 remains sole material |
/// | M9 | C3 false | - | 400/no source |
#[tokio::test]
async fn hosted_application_bearer_http_contract_is_stable_and_rotatable() {
    let credentials = Arc::new(InMemoryCredentialRepo::new());
    let secrets = Arc::new(InMemorySecretStore::new());
    let state = Arc::new(VaultState::new(
        secrets.clone(),
        credentials.clone(),
        awaken_time::system_clock(),
        None,
    ));
    let app = application_mcp_credentials_router(state.clone());
    let body = |url: &str, generation: u64, token: &str| {
        json!({
            "application_authority_id": "awaken-flow",
            "mcp_server_url": url,
            "credential_generation": generation,
            "token": token // awaken-allow(secret):
        })
    };

    assert_eq!(
        call(
            &app,
            "workspace-a",
            Some("flow-bearer-zero"),
            body("https://flow.example.test/mcp", 0, "token-zero")
        )
        .await
        .0,
        StatusCode::BAD_REQUEST,
        "M0"
    );
    assert!(
        credentials.list("workspace-a").await.unwrap().is_empty(),
        "M0"
    );
    assert!(secrets.inventory().await.unwrap().is_empty(), "M0");
    assert_eq!(
        call(
            &app,
            "workspace-a",
            None,
            body("https://flow.example.test/mcp", 1, "token-1")
        )
        .await
        .0,
        StatusCode::BAD_REQUEST,
        "M9"
    );
    let (status, created) = call(
        &app,
        "workspace-a",
        Some("flow-bearer-1"),
        body("HTTPS://FLOW.EXAMPLE.TEST:443/mcp/", 1, "token-1"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "M1");
    assert_eq!(created["revision"], 1);
    assert_eq!(created["adoption"], "converged", "M1/E6");
    assert!(!created.to_string().contains("token-1"), "M1/E5");
    let source_id =
        CredentialSourceId(created["credential_source_id"].as_str().unwrap().to_owned());
    assert!(
        credentials
            .get(&source_id)
            .await
            .unwrap()
            .material_ref
            .unwrap()
            .0
            .contains(":generation:1:attempt:"),
        "M1 generation precedes the existing writer-attempt fence"
    );

    let (_, replay) = call(
        &app,
        "workspace-a",
        Some("flow-bearer-1"),
        body("https://flow.example.test/mcp", 1, "token-1"),
    )
    .await;
    assert_eq!(replay, created, "M2");
    assert_eq!(
        call(
            &app,
            "workspace-a",
            Some("flow-bearer-1"),
            body("https://flow.example.test/mcp", 1, "different-token")
        )
        .await
        .0,
        StatusCode::CONFLICT,
        "M3"
    );
    assert_eq!(
        call(
            &app,
            "workspace-a",
            Some("flow-bearer-conflict"),
            body("https://flow.example.test/mcp", 1, "other-token")
        )
        .await
        .0,
        StatusCode::CONFLICT,
        "M3"
    );
    let (status, pending_without_target) = call(
        &app,
        "workspace-a",
        Some("flow-bearer-2"),
        body("https://flow.example.test/mcp", 2, "token-2"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "M4");
    assert_eq!(pending_without_target["vault_id"], created["vault_id"]);
    assert_eq!(
        pending_without_target["credential_source_id"],
        created["credential_source_id"]
    );
    assert_eq!(pending_without_target["revision"], 2);
    assert_eq!(pending_without_target["adoption"], "pending", "M4/E6");
    assert!(
        !pending_without_target.to_string().contains("token-2"),
        "M4/E5"
    );
    assert_eq!(
        credentials.pending_managed_rollouts().await.unwrap().len(),
        1,
        "M4 retains the exact durable event"
    );

    let target = Arc::new(AdoptionTarget::default());
    target.pending.store(true, Ordering::SeqCst);
    state.application().set_rollout_target(target.clone());
    let (_, pending_target) = call(
        &app,
        "workspace-a",
        Some("flow-bearer-2"),
        body("https://flow.example.test/mcp", 2, "token-2"),
    )
    .await;
    assert_eq!(pending_target["revision"], 2, "M5 does not rotate again");
    assert_eq!(pending_target["adoption"], "pending", "M5/E6");

    target.pending.store(false, Ordering::SeqCst);
    target.fail.store(true, Ordering::SeqCst);
    let (_, failed_target) = call(
        &app,
        "workspace-a",
        Some("flow-bearer-2"),
        body("https://flow.example.test/mcp", 2, "token-2"),
    )
    .await;
    assert_eq!(failed_target["revision"], 2, "M5 preserves the revision");
    assert_eq!(failed_target["adoption"], "pending", "M5/E6");

    target.fail.store(false, Ordering::SeqCst);
    let (_, converged_replay) = call(
        &app,
        "workspace-a",
        Some("flow-bearer-2"),
        body("https://flow.example.test/mcp", 2, "token-2"),
    )
    .await;
    assert_eq!(converged_replay["revision"], 2, "M6 exact replay");
    assert_eq!(converged_replay["adoption"], "converged", "M6/E6");
    assert!(
        credentials
            .pending_managed_rollouts()
            .await
            .unwrap()
            .is_empty(),
        "M6 acknowledges only the exact converged event"
    );

    let (_, converged_rotation) = call(
        &app,
        "workspace-a",
        Some("flow-bearer-3"),
        body("https://flow.example.test/mcp", 3, "token-3"),
    )
    .await;
    assert_eq!(converged_rotation["revision"], 3, "M7/E4");
    assert_eq!(converged_rotation["adoption"], "converged", "M7/E6");
    assert!(!converged_rotation.to_string().contains("token-3"), "M7/E5");
    assert!(
        credentials
            .pending_managed_rollouts()
            .await
            .unwrap()
            .is_empty(),
        "M7 exact convergence leaves no pending event"
    );
    let target_attempts_before_stale = target.events.lock().unwrap().len();
    assert_eq!(
        call(
            &app,
            "workspace-a",
            Some("flow-bearer-1"),
            body("https://flow.example.test/mcp", 1, "token-1")
        )
        .await
        .0,
        StatusCode::CONFLICT,
        "M8"
    );
    assert_eq!(
        target.events.lock().unwrap().len(),
        target_attempts_before_stale,
        "M8 reaches neither rollout target nor a second effect"
    );
    let durable = credentials.get(&source_id).await.unwrap();
    assert_eq!(durable.version, 3, "M8");
    assert_eq!(
        secrets
            .get(durable.material_ref.as_ref().unwrap())
            .await
            .unwrap()
            .expose_secret(),
        "token-3",
        "M8 does not restore delayed material"
    );
    assert_eq!(secrets.inventory().await.unwrap().len(), 1, "M8/E3");
}
