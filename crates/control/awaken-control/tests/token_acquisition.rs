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
    (
        axum::http::StatusCode::OK,
        Json(if state.nested {
            json!({"ok":true,"data":{"bearer":"issued-access","renewal":"issued-refresh","ttl":"3600","renewal_ttl":"86400"}})
        } else {
            json!({"access_token":"issued-access","refresh_token":"issued-refresh","expires_in":3600,"refresh_token_expires_in":86400}) // awaken-allow(secret): synthetic local issuer response
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
async fn source(
    repo: &InMemoryCredentialRepo,
    secrets: &InMemorySecretStore,
    profile: CredentialTokenExchangeProfile,
) -> CredentialSource {
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
