use super::*;
use awaken_credential_contract::TokenResponseCondition;
use serde_json::{Value, json};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize};

struct Clock(AtomicU64);
impl WallClock for Clock {
    fn now(&self) -> awaken_time::UtcTime {
        awaken_time::UtcTime::from_epoch_ms(self.0.load(Ordering::Acquire))
    }
}
fn spec() -> TokenResponseSpec {
    TokenResponseSpec {
        access_ttl_unit: awaken_credential_contract::TokenExpiryUnit::Seconds,
        additional_fields: Default::default(),
        access_token_pointer: "/access_token".into(),
        access_ttl_pointer: Some("/expires_in".into()),
        require_access_ttl: true,
        refresh_token_pointer: None,
        refresh_ttl_pointer: None,
        require_refresh_ttl: false,
        refresh_policy: RefreshTokenPolicy::NotExpected,
        success: vec![],
        authorization_required: vec![],
        max_access_ttl_seconds: 86400,
        max_refresh_ttl_seconds: 2592000,
        refresh_margin_ms: 1000,
    }
}
struct Port {
    replies: Mutex<Vec<Value>>,
    issued: AtomicUsize,
    published: AtomicUsize,
    revoked: AtomicBool,
    durable: bool,
}
impl Port {
    fn new(replies: Vec<Value>) -> Self {
        Self {
            replies: Mutex::new(replies.into_iter().rev().collect()),
            issued: AtomicUsize::new(0),
            published: AtomicUsize::new(0),
            revoked: AtomicBool::new(false),
            durable: false,
        }
    }
}
#[async_trait]
impl CredentialTokenIssuer for Port {
    async fn authorize(&self) -> Result<(), TokenAcquisitionError> {
        if self.revoked.load(Ordering::Acquire) {
            Err(TokenAcquisitionError::AuthorityDenied)
        } else {
            Ok(())
        }
    }
    async fn exchange(&self) -> Result<TokenEndpointResponse, TokenAcquisitionError> {
        self.issued.fetch_add(1, Ordering::AcqRel);
        tokio::task::yield_now().await;
        Ok(TokenEndpointResponse {
            status: 200,
            body: self
                .replies
                .lock()
                .unwrap()
                .pop()
                .expect("unexpected issuer request"),
        })
    }
    async fn publish(
        &self,
        tokens: IssuedCredentialTokens,
    ) -> Result<AcquiredCredentialToken, TokenAcquisitionError> {
        self.published.fetch_add(1, Ordering::AcqRel);
        Ok(AcquiredCredentialToken {
            additional_fields: tokens.additional_fields,
            access_token: tokens.access_token,
            expires_at_unix_ms: tokens.access_expires_at_unix_ms,
            reference: self
                .durable
                .then(|| awaken_agent_contract::credential::CredentialRef {
                    id: "replacement".into(),
                    revision: 1,
                }),
            cache_generation: None,
            cache_key: None,
            cache_partition: None,
        })
    }
    fn durable_publication(&self) -> bool {
        self.durable
    }
}
fn root(value: &str) -> Value {
    json!({"access_token":value,"expires_in":60})
}

#[tokio::test]
async fn exact_partition_single_flight_reauthorizes_and_expires_at_the_margin() {
    // A1 exact partition/spec + fresh grant -> one exchange for concurrent
    // consumers; A2 cache hit -> authority checked again; A3 equality at expiry
    // margin -> new exchange; A4 revoked source -> no cached use or new exchange.
    let clock = Arc::new(Clock(AtomicU64::new(1000)));
    let coordinator = Arc::new(TokenAcquisitionCoordinator::new(clock.clone()));
    let port = Arc::new(Port::new(vec![root("fixture-one"), root("fixture-two")]));
    let mut jobs = Vec::new();
    for _ in 0..12 {
        let (c, p) = (coordinator.clone(), port.clone());
        jobs.push(tokio::spawn(async move {
            c.acquire(Some("scope@1"), &spec(), p.as_ref())
                .await
                .unwrap()
        }));
    }
    for job in jobs {
        assert_eq!(
            job.await.unwrap().access_token.expose_secret(),
            "fixture-one"
        );
    }
    assert_eq!(port.issued.load(Ordering::Acquire), 1);
    clock.0.store(60000, Ordering::Release);
    assert_eq!(
        coordinator
            .acquire(Some("scope@1"), &spec(), port.as_ref())
            .await
            .unwrap()
            .access_token
            .expose_secret(),
        "fixture-two"
    );
    port.revoked.store(true, Ordering::Release);
    assert!(matches!(
        coordinator
            .acquire(Some("scope@1"), &spec(), port.as_ref())
            .await,
        Err(TokenAcquisitionError::AuthorityDenied)
    ));
    assert_eq!(port.issued.load(Ordering::Acquire), 2);
}

#[tokio::test]
async fn old_failure_cannot_invalidate_a_new_token_and_scope_or_spec_changes_do_not_share() {
    // C1 first token fails -> invalidate its exact cache generation. C2 a new
    // token is issued -> delayed old response cannot evict it. C3 another scope
    // or response contract -> separate issuance even with otherwise equal data.
    let coordinator = TokenAcquisitionCoordinator::new(Arc::new(Clock(AtomicU64::new(1000))));
    let port = Port::new(vec![
        root("one"),
        root("two"),
        root("other-scope"),
        root("other-contract"),
    ]);
    let old = coordinator
        .acquire(Some("workspace-a"), &spec(), &port)
        .await
        .unwrap();
    coordinator.invalidate("workspace-a", &old);
    let fresh = coordinator
        .acquire(Some("workspace-a"), &spec(), &port)
        .await
        .unwrap();
    coordinator.invalidate("workspace-a", &old);
    assert_eq!(
        coordinator
            .acquire(Some("workspace-a"), &spec(), &port)
            .await
            .unwrap()
            .access_token
            .expose_secret(),
        fresh.access_token.expose_secret()
    );
    coordinator
        .acquire(Some("workspace-b"), &spec(), &port)
        .await
        .unwrap();
    let mut changed = spec();
    changed.refresh_margin_ms = 0;
    coordinator
        .acquire(Some("workspace-a"), &changed, &port)
        .await
        .unwrap();
    assert_eq!(port.issued.load(Ordering::Acquire), 4);
}

#[tokio::test]
async fn rotating_profiles_require_publication_before_any_issuer_io() {
    // R1 rotating declaration + ephemeral publisher -> refuse before HTTP.
    // R2 exact durable publisher -> decode paired nested tokens and return only
    // the published access lease/reference. R3 malformed success -> uncertainty.
    let coordinator = TokenAcquisitionCoordinator::new(Arc::new(Clock(AtomicU64::new(1000))));
    let mut response = spec();
    response.access_token_pointer = "/data/token".into();
    response.access_ttl_pointer = Some("/data/ttl".into());
    response.refresh_policy = RefreshTokenPolicy::ReplaceRequired;
    response.refresh_token_pointer = Some("/data/renewal".into());
    response.refresh_ttl_pointer = Some("/data/renewal_ttl".into());
    response.require_refresh_ttl = true;
    response.success = vec![TokenResponseCondition {
        pointer: "/code".into(),
        equals: json!(0),
    }];
    let mut port = Port::new(vec![
        json!({"code":0,"data":{"token":"new-access","ttl":"60","renewal":"new-refresh","renewal_ttl":3600}}), // awaken-allow(secret): synthetic rotation response only
        json!({"code":0,"data":{"token":"incomplete","ttl":60}}),
    ]);
    assert!(matches!(
        coordinator.acquire(None, &response, &port).await,
        Err(TokenAcquisitionError::PublicationFailed)
    ));
    assert_eq!(port.issued.load(Ordering::Acquire), 0);
    port.durable = true;
    let issued = coordinator.acquire(None, &response, &port).await.unwrap();
    assert!(issued.reference.is_some());
    assert_eq!(issued.access_token.expose_secret(), "new-access");
    assert!(matches!(
        coordinator.acquire(None, &response, &port).await,
        Err(TokenAcquisitionError::OutcomeUnknown)
    ));
    assert_eq!(port.published.load(Ordering::Acquire), 1);
}

#[test]
fn response_contract_decision_table_rejects_malformed_expiry_and_secret_fields() {
    // D1 valid numeric/string TTL -> exact absolute expiry. D2 absent, null,
    // fractional, negative, zero, excessive or overflow TTL -> refusal. D3
    // absent/empty/nonstring/control-byte access -> refusal. Responses containing
    // arbitrary error text never become the error's display representation.
    for ttl in [json!(60), json!("60")] {
        let r = decode_token_response(
            &spec(),
            TokenEndpointResponse {
                status: 200,
                body: json!({"access_token":"ok","expires_in":ttl}),
            },
            1000,
        )
        .unwrap();
        assert_eq!(r.access_expires_at_unix_ms, Some(61000));
    }
    for ttl in [
        Value::Null,
        json!(-1),
        json!(0),
        json!(1.2),
        json!("-1"),
        json!(86401),
        json!("18446744073709551616"),
    ] {
        assert!(matches!(
            decode_token_response(
                &spec(),
                TokenEndpointResponse {
                    status: 200,
                    body: json!({"access_token":"ok","expires_in":ttl})
                },
                1000
            ),
            Err(TokenAcquisitionError::InvalidExpiry)
        ));
    }
    for token in [Value::Null, json!(""), json!(2), json!("not\na\nheader")] {
        assert!(matches!(
            decode_token_response(
                &spec(),
                TokenEndpointResponse {
                    status: 200,
                    body: json!({"access_token":token,"expires_in":60})
                },
                1000
            ),
            Err(TokenAcquisitionError::InvalidAccessToken)
        ));
    }
    let e = decode_token_response(
        &spec(),
        TokenEndpointResponse {
            status: 403,
            body: json!({"error":"untrusted-private-body"}),
        },
        1000,
    )
    .err()
    .unwrap();
    assert!(!e.to_string().contains("untrusted-private-body"));
    assert!(matches!(
        decode_token_response(
            &spec(),
            TokenEndpointResponse {
                status: 200,
                body: root("ok")
            },
            u64::MAX
        ),
        Err(TokenAcquisitionError::InvalidExpiry)
    ));
}

#[test]
fn optional_expiry_and_declared_authorization_failures_remain_explicit() {
    // E1 truly absent optional expiry -> no inferred lifetime; E2 a present
    // malformed expiry cannot hide behind optionality; E3 declared invalid-grant
    // condition -> authorization required even on an HTTP-success envelope.
    let mut response = spec();
    response.require_access_ttl = false;
    let issued = decode_token_response(
        &response,
        TokenEndpointResponse {
            status: 200,
            body: json!({"access_token":"opaque"}),
        },
        1000,
    )
    .unwrap();
    assert_eq!(issued.access_expires_at_unix_ms, None);
    assert!(
        decode_token_response(
            &response,
            TokenEndpointResponse {
                status: 200,
                body: json!({"access_token":"opaque","expires_in":"bad"})
            },
            1000
        )
        .is_err()
    );
    response.authorization_required = vec![TokenResponseCondition {
        pointer: "/error".into(),
        equals: json!("invalid_grant"),
    }];
    assert!(matches!(
        decode_token_response(
            &response,
            TokenEndpointResponse {
                status: 200,
                body: json!({"error":"invalid_grant"})
            },
            1000
        ),
        Err(TokenAcquisitionError::NeedsAuthorization)
    ));
}

#[tokio::test]
async fn legacy_multifield_capture_and_millisecond_ttl_use_the_same_decoder_and_cache() {
    // D4 seconds/milliseconds are declaration data: identical real lifetimes
    // produce the same absolute expiry. D5 every capture must be present and
    // bounded; a missing secondary capture cannot produce a partial lease.
    // D6 cache reuse retains ALL private fields, without another issuer call.
    let mut response = spec();
    response.access_ttl_unit = awaken_credential_contract::TokenExpiryUnit::Milliseconds;
    response.additional_fields = BTreeMap::from([("session".into(), "/session".into())]);
    let coordinator = TokenAcquisitionCoordinator::new(Arc::new(Clock(AtomicU64::new(1000))));
    let port = Port::new(vec![
        json!({"access_token":"access","session":"private-session","expires_in":60000}),
    ]);
    for _ in 0..2 {
        let token = coordinator
            .acquire(Some("capture"), &response, &port)
            .await
            .unwrap();
        assert_eq!(token.expires_at_unix_ms, Some(61000));
        assert_eq!(
            token.additional_fields["session"].expose_secret(),
            "private-session"
        );
    }
    assert_eq!(port.issued.load(Ordering::Acquire), 1);
    assert!(matches!(
        decode_token_response(
            &response,
            TokenEndpointResponse {
                status: 200,
                body: json!({"access_token":"access","expires_in":60000})
            },
            1000
        ),
        Err(TokenAcquisitionError::InvalidAccessToken)
    ));
}
