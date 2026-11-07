//! Generic request profiles through real HTTP and the real sealed generation
//! publisher. Provider identities and IAM discovery are explicit test fixtures.
use awaken_agent_contract::{
    RedactedString,
    credential::{
        AuthChallenge, CredentialRef, CredentialRefreshAccess, CredentialRefresher,
        TokenEndpointAuth,
    },
};
use awaken_control::{VaultRefreshFactory, VaultRefresher};
use awaken_credential_contract::{
    CredentialDescriptor, CredentialMaterialDescriptor, CredentialTargetContract,
    CredentialTokenExchangeProfile, RefreshTokenPolicy, TokenRequestEncoding,
    TokenResponseCondition, TokenResponseSpec, TokenSourceField,
};
use awaken_credential_vault::repo::{
    CredentialRepo, InMemoryCredentialRepo, token_publication_state,
};
use awaken_credential_vault::{
    CredentialCreateParams, CredentialKind, CredentialSource, InMemorySecretStore,
    OAUTH_CLIENT_SECRET_SLOT, OAUTH_REFRESH_TOKEN_SLOT, SecretStore,
};
use axum::{Json, Router, body::Bytes, extract::State, http::HeaderMap, routing::post};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

#[derive(Default)]
struct Issuer {
    calls: AtomicUsize,
    requests: Mutex<Vec<(String, Vec<u8>)>>,
    nested: bool,
    mode: AtomicUsize,
}
async fn issue(
    State(state): State<Arc<Issuer>>,
    headers: HeaderMap,
    body: Bytes,
) -> (axum::http::StatusCode, Json<Value>) {
    state.calls.fetch_add(1, Ordering::AcqRel);
    state.requests.lock().unwrap().push((
        headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned(),
        body.to_vec(),
    ));
    tokio::task::yield_now().await;
    if state.mode.load(Ordering::Acquire) == 1 {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({"error":"invalid_grant"})),
        );
    }
    if state.mode.load(Ordering::Acquire) == 2 {
        return (
            axum::http::StatusCode::OK,
            Json(json!({"access_token":"partial-issued-response"})), // awaken-allow(secret): synthetic local issuer response
        );
    }
    let ttl = if state.mode.load(Ordering::Acquire) == 3 {
        10
    } else {
        3600
    };
    (
        axum::http::StatusCode::OK,
        Json(if state.nested {
            json!({"ok":true,"data":{"bearer":"issued-access","renewal":"issued-refresh","ttl":ttl.to_string(),"renewal_ttl":"86400"}})
        } else {
            json!({"access_token":"issued-access","refresh_token":"issued-refresh","expires_in":ttl,"refresh_token_expires_in":86400}) // awaken-allow(secret): synthetic local issuer response
        }),
    )
}
fn profile(endpoint: String, nested: bool) -> CredentialTokenExchangeProfile {
    let r = |field: &str| json!({"credential":"grant","field":field});
    CredentialTokenExchangeProfile {
        method: awaken_credential_contract::TokenRequestMethod::Post,
        version: 1,
        token_endpoint: endpoint,
        encoding: if nested {
            TokenRequestEncoding::Json
        } else {
            TokenRequestEncoding::Form
        },
        parameters: if nested {
            json!({"app":{"id":123,"secret":r("secret")},"renewal":r("renewal")})
        } else {
            json!({"client_id":"public-client","client_secret":r("secret"),"refresh_token":r("renewal"),"grant_type":"refresh_token"})
        },
        headers: BTreeMap::new(),
        bindings: BTreeMap::from([(
            "grant".into(),
            BTreeMap::from([
                ("renewal".into(), TokenSourceField::RefreshToken {}),
                ("secret".into(), TokenSourceField::ClientSecret {}),
            ]),
        )]),
        basic_auth: None,
        access_field: None,
        response: TokenResponseSpec {
            access_ttl_unit: awaken_credential_contract::TokenExpiryUnit::Seconds,
            additional_fields: Default::default(),
            access_token_pointer: if nested {
                "/data/bearer"
            } else {
                "/access_token"
            }
            .into(),
            access_ttl_pointer: Some(if nested { "/data/ttl" } else { "/expires_in" }.into()),
            require_access_ttl: true,
            refresh_token_pointer: Some(
                if nested {
                    "/data/renewal"
                } else {
                    "/refresh_token"
                }
                .into(),
            ),
            refresh_ttl_pointer: Some(
                if nested {
                    "/data/renewal_ttl"
                } else {
                    "/refresh_token_expires_in"
                }
                .into(),
            ),
            require_refresh_ttl: true,
            refresh_policy: RefreshTokenPolicy::ReplaceRequired,
            success: if nested {
                vec![TokenResponseCondition {
                    pointer: "/ok".into(),
                    equals: json!(true),
                }]
            } else {
                vec![]
            },
            authorization_required: vec![TokenResponseCondition {
                pointer: "/error".into(),
                equals: json!("invalid_grant"),
            }],
            max_access_ttl_seconds: 86400,
            max_refresh_ttl_seconds: 2592000,
            refresh_margin_ms: 1000,
        },
    }
}
fn grant_descriptor(profile: CredentialTokenExchangeProfile) -> CredentialDescriptor {
    let target = awaken_agent_contract::credential::CredentialTarget::new(
        awaken_agent_contract::credential::CredentialPurpose::HttpEffect,
        "https://business.example",
    );
    let mut descriptor = CredentialDescriptor::new(
        "fixture",
        CredentialMaterialDescriptor::secret("awaken.secret/v1"),
        [CredentialTargetContract::new(
            target,
            awaken_agent_contract::credential::CredentialUsage::HttpEffect {
                fields: BTreeMap::from([(
                    "token".into(),
                    std::collections::BTreeSet::from([
                        awaken_agent_contract::credential::HttpEffectPlacement::Header {
                            name: "authorization".into(),
                        },
                    ]),
                )]),
            },
        )],
    );
    descriptor.acquisition = Some(profile);
    descriptor.validate().unwrap();
    descriptor
}
async fn source(
    repo: &InMemoryCredentialRepo,
    secrets: &InMemorySecretStore,
    profile: CredentialTokenExchangeProfile,
) -> CredentialSource {
    let descriptor = grant_descriptor(profile);
    use awaken_time::WallClockExt as _;
    let clock = awaken_time::system_clock();
    let now = clock.now_ms();
    let entry = awaken_credential_vault::repo::enter_described_credential(
        awaken_credential_vault::repo::DescribedCredentialEntryCommand {
            id: awaken_credential_contract::CredentialSourceId("fixture-grant".into()),
            params: CredentialCreateParams {
                workspace_id: "workspace".into(),
                kind: CredentialKind::Vault,
                provider_id: None,
                env_key: None,
                secret: Some(RedactedString::new("old-access")),
                oauth_command: None,
            },
            protocol_endpoint_id: None,
            descriptor,
            replacement_of: None,
            auxiliary_materials: BTreeMap::from([
                (
                    OAUTH_REFRESH_TOKEN_SLOT.into(),
                    RedactedString::new("old-refresh"),
                ),
                (
                    OAUTH_CLIENT_SECRET_SLOT.into(),
                    RedactedString::new("private-client"),
                ),
            ]),
            lifetime: Some(awaken_credential_contract::CredentialGrantLifetime {
                issued_at_unix_ms: now,
                access_expires_at_unix_ms: Some(now + 300000),
                refresh_expires_at_unix_ms: Some(now + 86400000),
            }),
        },
        secrets,
        repo,
        clock.as_ref(),
    )
    .await
    .unwrap();
    let serialized = serde_json::to_string(&entry.source).unwrap();
    assert!(
        !serialized.contains("old-access")
            && !serialized.contains("old-refresh")
            && !serialized.contains("private-client")
    );
    entry.source
}

#[tokio::test]
async fn freshly_issued_short_lifetime_does_not_request_another_immediate_renewal() {
    // Cause graph: imported grant, issued generation, requested validity and
    // profile margin -> whether another issuance can satisfy the same window.
    // L1 imported grant below requested validity -> Due (bootstrap allowed).
    // L2 issued lifetime <= requested validity/profile margin -> insufficient,
    // never another automatic Due even after rebuilding the observation owner.
    // L3 issued lifetime > window and sufficient time remains -> Ready.
    // Observation is metadata-only: all L2/L3 reads keep issuer count at one.
    // Constraint: the existing response decoder refuses TTL <= margin before
    // publication and retains OutcomeUnknown; observation cannot override it.
    // Decision table: valid margin + sufficient/insufficient requested window
    // -> Ready/L2; invalid margin + either window -> original OutcomeUnknown.
    for (margin, window, expected) in [
        (1_000, 1_000, "ready"),
        (1_000, 10_000, "insufficient_lifetime"),
        (20_000, 1_000, "outcome_unknown"),
        (20_000, 10_000, "outcome_unknown"),
    ] {
        let state = Arc::new(Issuer::default());
        state.mode.store(3, Ordering::Release);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/token", post(issue))
            .with_state(state.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let repo = Arc::new(InMemoryCredentialRepo::new());
        let secrets = Arc::new(InMemorySecretStore::new());
        let mut declaration = profile(endpoint, false);
        declaration.response.refresh_margin_ms = margin;
        let old = source(repo.as_ref(), secrets.as_ref(), declaration).await;
        let factory =
            VaultRefreshFactory::new(repo.clone(), secrets.clone(), awaken_time::system_clock());
        let old_ref = CredentialRef {
            id: old.id.0.clone(),
            revision: 1,
        };
        assert_eq!(
            serde_json::to_value(
                factory
                    .inspect_described("workspace", &old_ref, 600_000)
                    .await
                    .unwrap()
                    .unwrap()
            )
            .unwrap(),
            json!("due")
        );
        let renewed = factory
            .renew_described(
                "workspace",
                &old_ref,
                awaken_credential_contract::CredentialSourceId("short-replacement".into()),
            )
            .await;
        let reference = if expected == "outcome_unknown" {
            assert_eq!(
                renewed.unwrap_err(),
                awaken_credential_contract::TokenAcquisitionError::OutcomeUnknown
            );
            old_ref
        } else {
            let updated = renewed.unwrap();
            CredentialRef {
                id: updated.id.0,
                revision: 1,
            }
        };
        for _rebuilt_owner in 0..2 {
            let rebuilt = VaultRefreshFactory::new(
                repo.clone(),
                secrets.clone(),
                awaken_time::system_clock(),
            );
            assert_eq!(
                serde_json::to_value(
                    rebuilt
                        .inspect_described("workspace", &reference, window)
                        .await
                        .unwrap()
                        .unwrap()
                )
                .unwrap(),
                json!(expected),
                "L2/L3 margin={margin}, window={window}"
            );
        }
        assert!(
            factory
                .inspect_described("foreign-workspace", &reference, window)
                .await
                .is_err()
        );
        assert_eq!(state.calls.load(Ordering::Acquire), 1);
        server.abort();
    }
}

#[tokio::test]
async fn hosted_credential_api_preserves_private_grants_and_reuses_the_same_renewal_wal() {
    // Cause/effect table at the real HTTP boundary, with the ORIGINAL factory:
    // A1 described private pair + initial lifetime -> sealed create, metadata
    // only response and Due observation, zero issuer calls. A2 foreign scope,
    // stale version or changed descriptor -> reject before issuer dispatch.
    // A3 admitted renew -> one complete successor; lost-ACK replay and lookup
    // -> same receipt, one issuer call. A4 provider rejection -> typed conflict,
    // NeedsAuthorization and no re-redemption. A5 fresh write-only input using
    // that SAME operation preempts the rejected WAL; no secret enters replies.
    // Workspace stamping is fixture ingress; the actual Control PEP tests own
    // APIKEY_READ/WRITE and caller authorization for these same route families.
    use awaken_admin_config_api::{
        AdminState, EnterCredentialRequest, RenewCredentialRequest, admin_router,
    };
    use awaken_credential_contract::{CredentialGrantLifetime, CredentialMaterial};
    use awaken_time::WallClockExt as _;
    let issuer = Arc::new(Issuer::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let descriptor = grant_descriptor(profile(
        format!("http://{}/token", listener.local_addr().unwrap()),
        false,
    ));
    let app = Router::new()
        .route("/token", post(issue))
        .with_state(issuer.clone());
    let issuer_server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let repo = Arc::new(InMemoryCredentialRepo::new());
    let secrets = Arc::new(InMemorySecretStore::new());
    let clock = awaken_time::system_clock();
    let factory = Arc::new(VaultRefreshFactory::new(
        repo.clone(),
        secrets.clone(),
        clock.clone(),
    ));
    let api = admin_router(AdminState {
        catalog: Arc::new(awaken_model_catalog::repo::InMemoryCatalogRepo::new()),
        credentials: repo.clone(),
        secrets: secrets.clone(),
        profiles: Arc::new(awaken_config_resolver::InMemoryProfileStore::new()),
        resources: Arc::new(awaken_config_resolver::InMemoryAgentInputBindingRepository::new()),
        probe: None,
        grant_renewal: Some(factory),
        model_discovery: None,
        brokered_catalog: None,
        availability: Default::default(),
        clock: clock.clone(),
    })
    .layer(axum::Extension(
        awaken_runtime_contract::runs::scope::WorkspaceScope("workspace".into()),
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!(
        "http://{}/v1/config/credentials",
        listener.local_addr().unwrap()
    );
    let api_server = tokio::spawn(async move { axum::serve(listener, api).await.unwrap() });
    let client = reqwest::Client::new();
    let now = clock.now_ms();
    let inputs = || {
        BTreeMap::from([
            (
                OAUTH_REFRESH_TOKEN_SLOT.into(),
                RedactedString::new("rpc-old-refresh"),
            ),
            (
                OAUTH_CLIENT_SECRET_SLOT.into(),
                RedactedString::new("rpc-private-client"),
            ),
        ])
    };
    let create = EnterCredentialRequest::described_hosted_vault(
        "workspace".into(),
        "rpc-create".into(),
        descriptor.clone(),
        CredentialMaterial::secret(RedactedString::new("rpc-old-access")),
    )
    .unwrap()
    .with_grant_inputs(
        inputs(),
        Some(CredentialGrantLifetime {
            issued_at_unix_ms: now,
            access_expires_at_unix_ms: Some(now + 10_000),
            refresh_expires_at_unix_ms: Some(now + 3_600_000),
        }),
    )
    .unwrap();
    // A0 private inputs with no operation identity, no descriptor, or wrong
    // declared slots must fail before any source is published or issuer called.
    for (field, value) in [
        ("idempotency_key", Value::Null),
        ("descriptor", Value::Null),
        (
            "auxiliary_materials",
            json!({"unexpected":"fixture-private-value"}),
        ),
    ] {
        let mut invalid = serde_json::to_value(&create).unwrap();
        invalid[field] = value;
        assert_eq!(
            client
                .post(&base)
                .json(&invalid)
                .send()
                .await
                .unwrap()
                .status(),
            422,
            "A0 {field}"
        );
    }
    assert!(repo.list("workspace").await.unwrap().is_empty());
    assert_eq!(issuer.calls.load(Ordering::Acquire), 0);
    let created = client.post(&base).json(&create).send().await.unwrap();
    assert_eq!(created.status(), 201);
    let created: Value = created.json().await.unwrap();
    let original = CredentialRef {
        id: created["id"].as_str().unwrap().into(),
        revision: 1,
    };
    let observe = format!("{base}/{}/renewal", original.id);
    let view: Value = client
        .get(&observe)
        .query(&[("expected_version", 1u64), ("min_validity_ms", 30_000)])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(view["status"], "due");
    assert_eq!(issuer.calls.load(Ordering::Acquire), 0);
    let renewal = RenewCredentialRequest {
        workspace_id: "workspace".into(),
        expected_version: 1,
        idempotency_key: "rpc-renew".into(),
        descriptor: descriptor.clone(),
    };
    let renew_url = format!("{base}/{}/renew", original.id);
    for (field, value, status) in [
        ("workspace_id", json!("foreign"), 404),
        ("expected_version", json!(2), 409),
        (
            "descriptor",
            {
                let mut d = serde_json::to_value(&descriptor).unwrap();
                d["provider"] = json!("drift");
                d
            },
            422,
        ),
    ] {
        let mut rejected = serde_json::to_value(&renewal).unwrap();
        rejected[field] = value;
        assert_eq!(
            client
                .post(&renew_url)
                .json(&rejected)
                .send()
                .await
                .unwrap()
                .status()
                .as_u16(),
            status
        );
    }
    assert_eq!(issuer.calls.load(Ordering::Acquire), 0, "A2");
    let first = client.post(&renew_url).json(&renewal).send().await.unwrap();
    assert_eq!(first.status(), 201);
    let first: Value = first.json().await.unwrap();
    let replay: Value = client
        .post(&renew_url)
        .json(&renewal)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(first, replay, "A3");
    assert_eq!(
        first["replacement_of"],
        serde_json::to_value(&original).unwrap()
    );
    assert_ne!(first["id"], created["id"]);
    let recovered: Vec<Value> = client
        .get(&base)
        .query(&[
            ("provider_ref", "fixture"),
            ("idempotency_key", "rpc-renew"),
        ])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(recovered, vec![first.clone()]);
    let successor = CredentialRef {
        id: first["id"].as_str().unwrap().into(),
        revision: 1,
    };
    let stored = repo
        .get(&awaken_credential_contract::CredentialSourceId(
            successor.id.clone(),
        ))
        .await
        .unwrap();
    assert_eq!(
        secrets
            .get(stored.material_ref.as_ref().unwrap())
            .await
            .unwrap()
            .expose_secret(),
        "issued-access"
    );
    assert_eq!(
        secrets
            .get(&stored.auxiliary_material_refs[OAUTH_REFRESH_TOKEN_SLOT])
            .await
            .unwrap()
            .expose_secret(),
        "issued-refresh"
    );
    assert_eq!(issuer.calls.load(Ordering::Acquire), 1);
    issuer.mode.store(1, Ordering::Release);
    let rejected_url = format!("{base}/{}/renew", successor.id);
    let rejected = RenewCredentialRequest {
        workspace_id: "workspace".into(),
        expected_version: 1,
        idempotency_key: "rpc-rejected".into(),
        descriptor: descriptor.clone(),
    };
    for _retry in 0..2 {
        assert_eq!(
            client
                .post(&rejected_url)
                .json(&rejected)
                .send()
                .await
                .unwrap()
                .status(),
            409
        );
    }
    assert_eq!(issuer.calls.load(Ordering::Acquire), 2, "A4");
    let view: Value = client
        .get(format!("{base}/{}/renewal", successor.id))
        .query(&[("expected_version", 1u64), ("min_validity_ms", 30_000)])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(view["status"], "needs_authorization");
    let reauthorize = EnterCredentialRequest::described_hosted_vault(
        "workspace".into(),
        "rpc-rejected".into(),
        descriptor,
        CredentialMaterial::secret(RedactedString::new("rpc-new-access")),
    )
    .unwrap()
    .with_replacement_of(successor.clone())
    .unwrap()
    .with_grant_inputs(
        BTreeMap::from([
            (
                OAUTH_REFRESH_TOKEN_SLOT.into(),
                RedactedString::new("rpc-new-refresh"),
            ),
            (
                OAUTH_CLIENT_SECRET_SLOT.into(),
                RedactedString::new("rpc-new-client"),
            ),
        ]),
        None,
    )
    .unwrap();
    let authorized = client.post(&base).json(&reauthorize).send().await.unwrap();
    assert_eq!(authorized.status(), 201, "A5");
    let authorized: Value = authorized.json().await.unwrap();
    assert_eq!(issuer.calls.load(Ordering::Acquire), 2);
    assert_eq!(
        authorized["replacement_of"],
        serde_json::to_value(successor).unwrap()
    );
    for body in [created, first, authorized] {
        let public = serde_json::to_string(&body).unwrap();
        for private in [
            "rpc-old-refresh",
            "rpc-private-client",
            "rpc-old-access",
            "rpc-new-access",
            "rpc-new-refresh",
            "rpc-new-client",
            "issued-access",
            "issued-refresh",
            "material_ref",
            "auxiliary_material_refs",
        ] {
            assert!(
                !public.contains(private),
                "public receipt contains private data"
            );
        }
    }
    api_server.abort();
    issuer_server.abort();
}

#[tokio::test]
async fn two_protocol_profiles_use_one_executor_and_publish_complete_replacement_generations() {
    // P1 form/root + P2 JSON/nested are DATA variants of the same executor.
    // Exact current source -> complete new source; replay -> same receipt, no
    // issuer repeat; old source/material remain available until its owner relinks.
    for nested in [false, true] {
        let state = Arc::new(Issuer {
            nested,
            ..Issuer::default()
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/token", post(issue))
            .with_state(state.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let repo = Arc::new(InMemoryCredentialRepo::new());
        let secrets = Arc::new(InMemorySecretStore::new());
        let old = source(repo.as_ref(), secrets.as_ref(), profile(endpoint, nested)).await;
        let factory =
            VaultRefreshFactory::new(repo.clone(), secrets.clone(), awaken_time::system_clock());
        let reference = CredentialRef {
            id: old.id.0.clone(),
            revision: 1,
        };
        let candidate =
            awaken_credential_contract::CredentialSourceId(format!("replacement-{nested}"));
        let updated = factory
            .renew_described("workspace", &reference, candidate.clone())
            .await
            .unwrap();
        assert_eq!(updated.replacement_of, Some(reference.clone()));
        assert_eq!(updated.version, 1);
        assert_eq!(
            secrets
                .get(updated.material_ref.as_ref().unwrap())
                .await
                .unwrap()
                .expose_secret(),
            "issued-access"
        );
        assert_eq!(
            secrets
                .get(
                    updated
                        .auxiliary_material_ref(OAUTH_REFRESH_TOKEN_SLOT)
                        .unwrap()
                )
                .await
                .unwrap()
                .expose_secret(),
            "issued-refresh"
        );
        let published = token_publication_state(&updated, secrets.as_ref())
            .await
            .unwrap()
            .unwrap();
        assert!(published.access_expires_at_unix_ms.is_some());
        assert_eq!(
            factory
                .renew_described("workspace", &reference, candidate)
                .await
                .unwrap(),
            updated
        );
        assert_eq!(state.calls.load(Ordering::Acquire), 1);
        assert_eq!(repo.get(&old.id).await.unwrap(), old);
        assert_eq!(
            secrets
                .get(old.material_ref.as_ref().unwrap())
                .await
                .unwrap()
                .expose_secret(),
            "old-access"
        );
        let requests = state.requests.lock().unwrap();
        if nested {
            let body: Value = serde_json::from_slice(&requests[0].1).unwrap();
            assert_eq!(body["app"]["id"], 123);
            assert_eq!(body["renewal"], "old-refresh");
        } else {
            let body = String::from_utf8(requests[0].1.clone()).unwrap();
            assert!(body.contains("grant_type=refresh_token"));
            assert!(body.contains("refresh_token=old-refresh"));
        }
        server.abort();
    }
}

#[tokio::test]
async fn concurrent_refresh_factories_share_the_durable_source_claim() {
    // C1 separate factory/cache instances + the same source revision -> ONE
    // remote redemption. Followers adopt only the direct publication receipt;
    // source CAS alone AFTER exchange would not satisfy this rule.
    let state = Arc::new(Issuer::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/token", post(issue))
        .with_state(state.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let repo = Arc::new(InMemoryCredentialRepo::new());
    let secrets = Arc::new(InMemorySecretStore::new());
    let old = source(
        repo.as_ref(),
        secrets.as_ref(),
        profile(endpoint.clone(), false),
    )
    .await;
    let access = CredentialRefreshAccess::new(
        1,
        endpoint,
        "public-client".into(),
        TokenEndpointAuth::ClientSecretPost,
        old.auxiliary_material_ref(OAUTH_CLIENT_SECRET_SLOT)
            .map(|v| v.0.clone()),
        old.auxiliary_material_ref(OAUTH_REFRESH_TOKEN_SLOT)
            .unwrap()
            .0
            .clone(),
        old.material_ref.as_ref().unwrap().0.clone(),
        None,
        None,
    );
    let mut jobs = Vec::new();
    for _ in 0..8 {
        let refresher = VaultRefresher::new(
            old.id.clone(),
            access.clone(),
            repo.clone(),
            secrets.clone(),
            awaken_time::system_clock(),
        );
        jobs.push(tokio::spawn(async move {
            refresher
                .refresh(&AuthChallenge {
                    status: 401,
                    www_authenticate: None,
                })
                .await
        }));
    }
    for job in jobs {
        assert!(job.await.unwrap().is_some());
    }
    assert_eq!(state.calls.load(Ordering::Acquire), 1);
    assert_eq!(repo.get(&old.id).await.unwrap().version, 2);
    server.abort();
}

#[tokio::test]
async fn failed_redemption_survives_factory_restart_and_fresh_authorization_preempts_it() {
    // F1 explicit invalid_grant -> NeedsAuthorization; F2 malformed 2xx that
    // may already have rotated a grant -> OutcomeUnknown. F3 rebuilding the
    // entire factory must preserve both terminal observations and make zero
    // extra requests. F4 explicit private reauthorization at the SAME command
    // identity replaces that pending attempt through the original material WAL.
    for mode in [1, 2] {
        let state = Arc::new(Issuer::default());
        state.mode.store(mode, Ordering::Release);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/token", post(issue))
            .with_state(state.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let repo = Arc::new(InMemoryCredentialRepo::new());
        let secrets = Arc::new(InMemorySecretStore::new());
        let old = source(repo.as_ref(), secrets.as_ref(), profile(endpoint, false)).await;
        let reference = CredentialRef {
            id: old.id.0.clone(),
            revision: 1,
        };
        let candidate = awaken_credential_contract::CredentialSourceId(format!("recovery-{mode}"));
        let expected = if mode == 1 {
            awaken_credential_contract::CredentialRenewalStatus::NeedsAuthorization
        } else {
            awaken_credential_contract::CredentialRenewalStatus::OutcomeUnknown
        };
        for _ in 0..2 {
            let factory = VaultRefreshFactory::new(
                repo.clone(),
                secrets.clone(),
                awaken_time::system_clock(),
            );
            assert!(
                factory
                    .renew_described("workspace", &reference, candidate.clone())
                    .await
                    .is_err()
            );
            assert_eq!(
                factory
                    .inspect_described("workspace", &reference, 30_000)
                    .await
                    .unwrap(),
                Some(expected)
            );
        }
        assert_eq!(
            state.calls.load(Ordering::Acquire),
            1,
            "F3 no retry after restart"
        );
        assert!(
            repo.get(&candidate).await.is_err(),
            "partial material is never published"
        );
        let entry = awaken_credential_vault::repo::enter_described_credential(
            awaken_credential_vault::repo::DescribedCredentialEntryCommand {
                id: candidate.clone(),
                params: CredentialCreateParams {
                    workspace_id: "workspace".into(),
                    kind: CredentialKind::Vault,
                    provider_id: None,
                    env_key: None,
                    secret: Some(RedactedString::new("fresh-authorized-access")),
                    oauth_command: None,
                },
                protocol_endpoint_id: None,
                descriptor: old.descriptor.clone().unwrap(),
                replacement_of: Some(reference),
                auxiliary_materials: BTreeMap::from([
                    (
                        OAUTH_REFRESH_TOKEN_SLOT.into(),
                        RedactedString::new("fresh-authorized-refresh"),
                    ),
                    (
                        OAUTH_CLIENT_SECRET_SLOT.into(),
                        RedactedString::new("private-client"),
                    ),
                ]),
                lifetime: None,
            },
            secrets.as_ref(),
            repo.as_ref(),
            awaken_time::system_clock().as_ref(),
        )
        .await
        .unwrap();
        assert_eq!(entry.source.id, candidate);
        assert!(repo.pending_mutation(&candidate).await.unwrap().is_none());
        assert_eq!(
            state.calls.load(Ordering::Acquire),
            1,
            "F4 reauthorization never redeems the old grant"
        );
        server.abort();
    }
}
