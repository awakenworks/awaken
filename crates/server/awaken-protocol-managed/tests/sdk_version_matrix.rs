//! T30-C01: every pinned official SDK version's request witnesses decode
//! through the one Rust capability registry and reach the real router, and the
//! Rust wire schema is the committed generated snapshot.
//!
//! Witnesses come only from the oracle's generated contracts:
//! `operation-coverage.generated.json` names, per operation, the exact SDK
//! anchors that can issue it and the beta headers it sends;
//! `upstream-oracle.generated.json` names each pinned anchor version and the
//! operations only the current anchor has.
//!
//! | Rule | witness | Effect |
//! |---|---|---|
//! | V1 | each anchor's operations and betas | each beta selects exactly one `ManagedCapability` |
//! | V2 | anchor operation sets vs. `only_in_current` | an anchor never claims an operation it lacks |
//! | V3 | each anchor's Session operations, its own betas | the real router owns the route |
//! | V4 | the Rust schema export | equals `canonical-wire.schemas.generated.json` |
//! | NC1 | a prefix-matching registry | V1 rejects it |
//! | NC2 | a route outside the router | V3's ownership check reports the miss |
//! | NC3 | a renamed wire field | V4 rejects it |

mod support;

use std::collections::BTreeSet;
use std::time::Duration;

use awaken_protocol_managed::ManagedCapability;
use awaken_protocol_managed_testkit::CoordinatedRuntimeFake;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;

fn upstream_oracle() -> Value {
    serde_json::from_str(include_str!(
        "../../../../contracts/anthropic-managed/upstream-oracle.generated.json"
    ))
    .expect("generated upstream oracle is valid JSON")
}

fn operation_coverage() -> Value {
    serde_json::from_str(include_str!(
        "../../../../contracts/anthropic-managed/operation-coverage.generated.json"
    ))
    .expect("generated operation coverage is valid JSON")
}

/// One pinned SDK anchor: its id and exact package version.
fn anchors() -> Vec<(String, String)> {
    upstream_oracle()["anchors"]
        .as_array()
        .expect("anchors are generated")
        .iter()
        .map(|anchor| {
            (
                anchor["id"].as_str().expect("anchor id").to_owned(),
                anchor["version"]
                    .as_str()
                    .expect("anchor version")
                    .to_owned(),
            )
        })
        .collect()
}

/// One request witness an anchor's SDK can issue.
struct Witness {
    id: String,
    method: String,
    path: String,
    betas: Vec<String>,
}

fn witnesses(anchor: &str) -> Vec<Witness> {
    operation_coverage()["operations"]
        .as_array()
        .expect("operations are generated")
        .iter()
        .filter(|operation| {
            operation["sdk_anchors"]
                .as_array()
                .is_some_and(|anchors| anchors.iter().any(|value| value == anchor))
        })
        .map(|operation| Witness {
            id: operation["id"].as_str().expect("operation id").to_owned(),
            method: operation["method"].as_str().expect("method").to_owned(),
            path: operation["path"].as_str().expect("path").to_owned(),
            betas: operation["betas"]
                .as_array()
                .expect("betas")
                .iter()
                .map(|beta| beta.as_str().expect("beta").to_owned())
                .collect(),
        })
        .collect()
}

/// The production registry decode: every capability whose beta is the token.
fn registry_decode(beta: &str) -> Vec<ManagedCapability> {
    ManagedCapability::ALL
        .into_iter()
        .filter(|capability| capability.beta() == beta)
        .collect()
}

/// The V1 oracle for one decoder: the witnesses whose beta does not select
/// exactly one capability.
fn ambiguous_witnesses(decode: impl Fn(&str) -> Vec<ManagedCapability>) -> Vec<String> {
    let mut ambiguous = Vec::new();
    for (anchor, _) in anchors() {
        for witness in witnesses(&anchor) {
            for beta in &witness.betas {
                let selected = decode(beta);
                if selected.len() != 1 {
                    ambiguous.push(format!("{anchor}:{}:{beta}:{}", witness.id, selected.len()));
                }
            }
        }
    }
    ambiguous
}

#[test]
fn every_pinned_version_decodes_through_the_one_capability_registry() {
    let pinned = anchors();
    assert_eq!(
        pinned.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
        [
            "oldest",
            "user_profiles_legacy",
            "beta_resources_legacy",
            "current"
        ],
        "the pinned anchor set is explicit"
    );
    for (anchor, version) in &pinned {
        assert!(
            !witnesses(anchor).is_empty(),
            "V1 {anchor}@{version} has request witnesses"
        );
    }
    assert_eq!(
        ambiguous_witnesses(registry_decode),
        Vec::<String>::new(),
        "V1 every pinned beta selects exactly one capability"
    );

    // NC1: a registry that matches the beta family prefix instead of the
    // exact dated token conflates the legacy and current capabilities.
    let prefix_decode = |beta: &str| -> Vec<ManagedCapability> {
        let family = |token: &str| token.rsplitn(4, '-').last().unwrap_or(token).to_owned();
        ManagedCapability::ALL
            .into_iter()
            .filter(|capability| family(capability.beta()) == family(beta))
            .collect()
    };
    assert!(
        !ambiguous_witnesses(prefix_decode).is_empty(),
        "NC1 a prefix-matching registry is detected"
    );
}

#[test]
fn no_pinned_version_claims_an_operation_it_lacks() {
    let oracle = upstream_oracle();
    for anchor in oracle["anchors"].as_array().expect("anchors") {
        let id = anchor["id"].as_str().expect("anchor id");
        let lacking: BTreeSet<&str> = anchor["only_in_current"]
            .as_array()
            .expect("only_in_current is generated")
            .iter()
            .map(|operation| operation.as_str().expect("operation id"))
            .collect();
        let claimed: BTreeSet<String> = witnesses(id).into_iter().map(|w| w.id).collect();
        let contradicted: Vec<&&str> = lacking
            .iter()
            .filter(|operation| claimed.contains(**operation))
            .collect();
        assert!(
            contradicted.is_empty(),
            "V2 {id} claims operations only the current SDK has: {contradicted:?}"
        );
    }
}

/// Whether the router owns `method path`: axum answers an unrouted path or
/// method with an empty 404/405, while every owned route answers with its
/// handler's or extractor's own body.
async fn router_owns(app: &axum::Router, method: &str, path: &str, betas: &[String]) -> bool {
    let method = Method::from_bytes(method.as_bytes()).expect("generated method");
    let mut request = Request::builder().method(method.clone()).uri(path).header(
        "anthropic-version",
        awaken_protocol_managed::ANTHROPIC_API_VERSION,
    );
    if !betas.is_empty() {
        request = request.header("anthropic-beta", betas.join(","));
    }
    let body = if method == Method::POST || method == Method::PATCH {
        request = request.header("content-type", "application/json");
        Body::from("{}")
    } else {
        Body::empty()
    };
    let response = tokio::time::timeout(
        Duration::from_secs(30),
        app.clone().oneshot(request.body(body).expect("request")),
    )
    .await
    .expect("the router answers in time")
    .expect("infallible router");
    let status = response.status();
    if status != StatusCode::NOT_FOUND && status != StatusCode::METHOD_NOT_ALLOWED {
        return true;
    }
    let bytes = tokio::time::timeout(Duration::from_secs(30), response.into_body().collect())
        .await
        .expect("the error body arrives in time")
        .expect("read the error body")
        .to_bytes();
    !bytes.is_empty()
}

#[tokio::test]
async fn every_pinned_version_reaches_the_session_router_with_its_own_betas() {
    let state = support::shared_state(CoordinatedRuntimeFake::default()).await;
    let app = support::edge_stamped(awaken_protocol_managed::router(state));
    for (anchor, version) in anchors() {
        let sessions: Vec<Witness> = witnesses(&anchor)
            .into_iter()
            .filter(|witness| witness.path.starts_with("/v1/sessions"))
            .collect();
        assert!(
            !sessions.is_empty(),
            "V3 {anchor}@{version} issues Session operations"
        );
        let mut unowned = Vec::new();
        for witness in &sessions {
            let path = witness.path.replace("{}", "sdkprobe");
            if !router_owns(&app, &witness.method, &path, &witness.betas).await {
                unowned.push(format!("{} {} ({})", witness.method, path, witness.id));
            }
        }
        assert!(
            unowned.is_empty(),
            "V3 {anchor}@{version} operations the router does not own: {unowned:?}"
        );
    }

    // NC2: the same ownership check reports a route the router does not have.
    assert!(
        !router_owns(
            &app,
            "GET",
            "/v1/sessions-not-a-route/sdkprobe",
            &[ManagedCapability::ManagedAgents.beta().to_owned()],
        )
        .await,
        "NC2 an unrouted witness is detected"
    );
}

#[cfg(feature = "schema")]
fn rust_wire_document() -> Value {
    let capabilities = ManagedCapability::ALL
        .map(|capability| serde_json::json!({ "id": capability.id(), "beta": capability.beta() }));
    serde_json::json!({
        "$comment": "GENERATED by awaken-protocol-managed export_schemas; do not edit.",
        "schema_version": 1,
        "protocol": {
            "anthropic_api_version": awaken_protocol_managed::ANTHROPIC_API_VERSION,
            "capabilities": capabilities,
        },
        "schemas": Value::Object(awaken_protocol_managed::schema::contract_schemas()),
    })
}

/// Rename the first property of the first schema that declares one, as an
/// internal serde rename would.
#[cfg(feature = "schema")]
fn rename_one_wire_field(document: &mut Value) -> String {
    let schemas = document["schemas"].as_object_mut().expect("schemas");
    for (name, schema) in schemas.iter_mut() {
        let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) else {
            continue;
        };
        let Some(field) = properties.keys().next().cloned() else {
            continue;
        };
        let value = properties.remove(&field).expect("field present");
        properties.insert(format!("{field}_renamed"), value);
        return format!("{name}.{field}");
    }
    panic!("no schema declares a property");
}

#[cfg(feature = "schema")]
#[test]
fn rust_wire_schema_is_the_committed_generated_contract() {
    let committed: Value = serde_json::from_str(include_str!(
        "../../../../contracts/anthropic-managed/canonical-wire.schemas.generated.json"
    ))
    .expect("committed canonical wire schema is valid JSON");
    let produced = rust_wire_document();
    assert!(
        produced == committed,
        "V4 the Rust wire schema drifted from canonical-wire.schemas.generated.json; \
         regenerate with scripts/contract/generate-contracts.sh and review the diff"
    );

    // NC3: one renamed field must fail the same equality.
    let mut renamed = produced;
    let field = rename_one_wire_field(&mut renamed);
    assert!(renamed != committed, "NC3 renaming {field} is detected");
}
