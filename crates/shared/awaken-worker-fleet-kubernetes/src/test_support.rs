// @formal surface exclude=test_fixture
// @formal owner crates/shared/awaken-worker-fleet-kubernetes/src/create_ledger/isolation.rs
// @formal reason: The opt-in in-process API fixture exposes observations and fault injection for
// @formal reason: tests of the production isolation guard; it grants no production admission authority.
//! Test support: an in-process Kubernetes API that serves the Create
//! isolation policy, one Pod and one PVC, for the isolation guard's own tests
//! and for the Sandbox adapter's guarded-claim tests that join the same guard.

use std::sync::Arc;

use awaken_sync::LeafLock;

use awaken_container_contract::{CreateTicket, CreateTicketRequest, TicketResult, TicketStatus};
use http::{Method, Request, Response};
use k8s_openapi::api::admissionregistration::v1::{
    ValidatingAdmissionPolicy, ValidatingAdmissionPolicyBinding,
};
use k8s_openapi::api::core::v1::Pod;
use kube::Client;
use kube::client::Body;
use serde_json::{Value, json};

use crate::create_ledger::isolation::{POLICY, expected_binding_spec, expected_policy_spec};

/// Isolate `ticket`'s Pod through the guard, as an original Create's join would.
pub async fn isolate_guard_pod(
    client: &Client,
    ticket: &CreateTicket,
) -> Result<Option<TicketResult>, awaken_container_contract::RuntimeError> {
    crate::create_ledger::isolation::isolate_pod(client, ticket).await
}

/// The observable state of the in-process API server.
#[derive(Default)]
pub struct IsolationApi {
    pub pod: Option<Pod>,
    pub claim: Option<k8s_openapi::api::core::v1::PersistentVolumeClaim>,
    pub claim_deletes: usize,
    pub claim_delete_response_lost: bool,
    pub policy: bool,
    pub policy_tampered: bool,
    pub binding_tampered: bool,
    pub policy_lagged: bool,
    pub delete_denied: bool,
    pub denial_code: u16,
    pub creates: usize,
}

/// One in-flight Pod Create ticket awaiting isolation.
pub fn isolation_ticket() -> CreateTicket {
    CreateTicket {
        request: CreateTicketRequest::new(
            "group",
            "pods",
            "sandbox-ns",
            "sandbox-pod",
            "a".repeat(64),
        )
        .unwrap(),
        status: TicketStatus::InFlight,
        origin: Default::default(),
        isolation_id: "0123456789abcdef0123456789abcdef".into(),
        isolation_pending: true,
    }
}

fn response(code: u16, body: Value) -> Response<Body> {
    Response::builder()
        .status(code)
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

type State = Arc<LeafLock<IsolationApi>>;

fn not_found(message: &str) -> Response<Body> {
    response(
        404,
        json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":"NotFound","message":message,"code":404}),
    )
}

/// A Kubernetes client whose API server is `state`. Any request outside the
/// isolation policy, the binding, one Pod and one PVC panics the test.
pub fn isolation_api(state: State) -> Client {
    let service = tower::service_fn(move |request: Request<Body>| {
        let state = state.clone();
        async move { Ok::<_, std::io::Error>(serve(&state, request).await) }
    });
    Client::new(service, "sandbox-ns")
}

async fn serve(state: &State, request: Request<Body>) -> Response<Body> {
    let path = request.uri().path().to_owned();
    let requested_pod = path
        .rsplit_once("/pods/")
        .map(|(_, name)| name.to_owned())
        .filter(|name| !name.is_empty() && !name.contains('/'));
    if path.ends_with("validatingadmissionpolicies/awaken-create-isolation") {
        policy(state)
    } else if path.ends_with("validatingadmissionpolicybindings/awaken-create-isolation") {
        binding(state)
    } else if path.ends_with("/pods") && request.method() == Method::POST {
        create_pod(state, request).await
    } else if let Some(name) = requested_pod {
        pod(state, request, &name).await
    } else if path.contains("/persistentvolumeclaims/") {
        claim(state, request).await
    } else {
        panic!(
            "unexpected isolation API request: {} {}",
            request.method(),
            path
        )
    }
}

fn policy(state: &State) -> Response<Body> {
    let state = state.lock();
    if !state.policy {
        return not_found("policy not found");
    }
    let mut spec = expected_policy_spec();
    if state.policy_tampered {
        spec.failure_policy = Some("Ignore".into());
    }
    let observed_generation = if state.policy_lagged { 0 } else { 1 };
    let policy = ValidatingAdmissionPolicy {
        metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
            name: Some(POLICY.into()),
            generation: Some(1),
            ..Default::default()
        },
        spec: Some(spec),
        status: Some(
            k8s_openapi::api::admissionregistration::v1::ValidatingAdmissionPolicyStatus {
                observed_generation: Some(observed_generation),
                ..Default::default()
            },
        ),
    };
    response(200, serde_json::to_value(policy).unwrap())
}

fn binding(state: &State) -> Response<Body> {
    let mut spec = expected_binding_spec();
    if state.lock().binding_tampered {
        spec.validation_actions = Some(vec!["Warn".into()]);
    }
    let binding = ValidatingAdmissionPolicyBinding {
        metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
            name: Some(POLICY.into()),
            ..Default::default()
        },
        spec: Some(spec),
    };
    response(200, serde_json::to_value(binding).unwrap())
}

async fn create_pod(state: &State, request: Request<Body>) -> Response<Body> {
    let bytes = request.into_body().collect_bytes().await.unwrap();
    let mut state = state.lock();
    state.creates += 1;
    if state.pod.is_some() {
        response(409, json!({"reason":"AlreadyExists","code":409}))
    } else {
        let mut pod: Pod = serde_json::from_slice(&bytes).unwrap();
        pod.metadata.uid = Some("sentinel-uid".into());
        state.pod = Some(pod.clone());
        response(201, serde_json::to_value(pod).unwrap())
    }
}

async fn pod(state: &State, request: Request<Body>, name: &str) -> Response<Body> {
    let method = request.method().clone();
    if method == Method::GET {
        return match &state.lock().pod {
            Some(pod) if pod.metadata.name.as_deref() == Some(name) => {
                response(200, serde_json::to_value(pod).unwrap())
            }
            None => not_found("Pod not found"),
            Some(_) => not_found("foreign Pod name"),
        };
    }
    let body = request.into_body().collect_bytes().await.unwrap();
    if method == Method::PUT {
        let mut replacement: Pod = serde_json::from_slice(&body).unwrap();
        let mut state = state.lock();
        let current = state.pod.as_ref().unwrap();
        assert_eq!(current.metadata.name.as_deref(), Some(name));
        if replacement.metadata.uid != current.metadata.uid
            || replacement.metadata.resource_version != current.metadata.resource_version
        {
            return response(
                409,
                json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":"Conflict","code":409}),
            );
        }
        replacement.metadata.resource_version = Some("2".into());
        state.pod = Some(replacement.clone());
        return response(200, serde_json::to_value(replacement).unwrap());
    }
    assert_eq!(
        method,
        Method::DELETE,
        "unexpected isolation Pod request {method}"
    );
    let state = state.lock();
    assert_eq!(
        state.pod.as_ref().unwrap().metadata.name.as_deref(),
        Some(name)
    );
    let options: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        options["dryRun"],
        json!(["All"]),
        "isolation probe must never perform a real DELETE"
    );
    assert_eq!(options["preconditions"]["uid"], "sentinel-uid");
    if state.delete_denied {
        let code = if state.denial_code == 0 {
            422
        } else {
            state.denial_code
        };
        response(
            code,
            json!({"reason":"Invalid","code":code,"message":format!("denied by {POLICY}")}),
        )
    } else {
        response(200, json!({"kind":"Status","status":"Success","code":200}))
    }
}

async fn claim(state: &State, request: Request<Body>) -> Response<Body> {
    let method = request.method().clone();
    if method == Method::GET {
        return match &state.lock().claim {
            Some(claim) => response(200, serde_json::to_value(claim).unwrap()),
            None => not_found("PVC not found"),
        };
    }
    assert_eq!(
        method,
        Method::DELETE,
        "unexpected isolation PVC request {method}"
    );
    let bytes = request.into_body().collect_bytes().await.unwrap();
    let options: Value = serde_json::from_slice(&bytes).unwrap();
    let mut state = state.lock();
    let claim = state.claim.as_ref().expect("exact original PVC exists");
    assert_eq!(options["preconditions"]["uid"], json!(claim.metadata.uid));
    assert_eq!(
        options["preconditions"]["resourceVersion"],
        json!(claim.metadata.resource_version)
    );
    state.claim_deletes += 1;
    state.claim = None;
    if state.claim_delete_response_lost {
        response(
            500,
            json!({"reason":"InternalError","code":500,"message":"response lost after accepted PVC DELETE"}),
        )
    } else {
        response(200, json!({"kind":"Status","status":"Success","code":200}))
    }
}
