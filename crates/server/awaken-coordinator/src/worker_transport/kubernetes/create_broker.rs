// @formal surface obligation=sandbox.admission_never_weakens_isolation
// @formal reason: The independent executor joins Registry identity and the
// @formal reason: provider CAS before forwarding a Kubernetes Create. Fault
// @formal reason: tests must still qualify its full physical closure boundary.
//! Independent Kubernetes Create executor behind the existing Worker identity.
//!
//! The signed Worker request owns no durable status. A ConfigMap ticket wins
//! CAS before a detached task calls the API. Losing the Worker HTTP response
//! cannot cancel that task; losing the Coordinator process leaves InFlight and
//! therefore Unknown rather than manufacturing a cleanup witness.

use std::sync::Arc;

use awaken_agent_contract::{
    collision_resistant_fingerprint, collision_resistant_fingerprint_digest,
};
use awaken_dispatch::{
    ApplicationError, RegisteredWorker, WorkerCreateLedger, WorkerDirectory, WorkerIdentity,
    WorkerState,
};
use awaken_sandbox_container::ContainerEffectFence;
use awaken_sandbox_container::k8s::{
    CreateReplyProfile, CreateTicketRequest, ISOLATION_ID_ANNOTATION, ISOLATION_LABEL,
    K8S_CREATE_WIRE_VERSION, K8sCreateRequest, K8sCreateResponse, K8sLedgerBinding,
    K8sWorkerCreateLedger, ORIGINAL_GUARD_FINALIZER, REQUEST_DIGEST_ANNOTATION, RequestJoin,
    TicketAdmission, TicketResult, collect_create_body, original_request_metadata_matches,
};
use awaken_time::{WallClock, WallClockExt};
use awaken_worker_transport_security::kubernetes::KubernetesPodIdentity;
use awaken_worker_transport_security::{
    VerifiedWorkerContext, WorkerRequestAuthenticator, authenticate_worker_request,
    verify_current_worker_identity, verify_registered_worker_identity,
};
use axum::extract::{Extension, State};
use axum::http::{Method, Request, StatusCode, header};
use axum::response::{IntoResponse, Response as AxumResponse};
use axum::routing::post;
use axum::{Json, Router};
use kube::Client;
use kube::client::Body;
use serde::Deserialize;
use serde_json::{Value, json};

mod response;
use response::BrokerCreateReply;

const PATH: &str = "/v1/worker/k8s/create";
const SEAL_PATH: &str = "/v1/worker/k8s/create/seal";
const REALIZATION_DIGEST: &str = "awaken.dev/realization-digest";

#[derive(Clone)]
pub(in crate::worker_transport) struct K8sCreateBroker {
    api_client: Client,
    ledger: K8sWorkerCreateLedger,
    sandbox_namespace: String,
    provider_id: String,
}

struct ValidatedCreate {
    path: String,
    body: Vec<u8>,
    ticket: CreateTicketRequest,
}

struct AdmittedCreate {
    create: ValidatedCreate,
    profile: CreateReplyProfile,
}

#[derive(Clone)]
struct CreateState {
    broker: Arc<K8sCreateBroker>,
    directory: Arc<dyn WorkerDirectory>,
    clock: Arc<dyn WallClock>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateWire {
    wire_version: u8,
    identity: WorkerIdentity,
    create: K8sCreateRequest,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SealWire {
    wire_version: u8,
    identity: WorkerIdentity,
    scope: String,
    predecessor: ContainerEffectFence,
    expected_pod_name: String,
}

fn status_response(code: u16, reason: &str) -> K8sCreateResponse {
    K8sCreateResponse {
        status: code,
        object: json!({
            "apiVersion": "v1", "kind": "Status", "status": "Failure",
            "reason": reason, "message": "brokered Create has no new API effect", "code": code
        }),
    }
}

fn current_create_worker(
    record: &RegisteredWorker,
    identity: &WorkerIdentity,
    anchor: Option<&WorkerCreateLedger>,
    now_ms: u64,
) -> bool {
    verify_registered_worker_identity(&record.snapshot, identity, now_ms, true).is_ok()
        && anchor.is_none_or(|anchor| record.create_ledger.as_ref() == Some(anchor))
}

fn original_readback_matches(
    object: &Value,
    ticket: &CreateTicketRequest,
    result: &TicketResult,
    nonce: &str,
) -> bool {
    let TicketResult::Created { uid } = result else {
        return false;
    };
    let metadata = &object["metadata"];
    result.validate().is_ok()
        && !nonce.is_empty()
        && metadata["uid"].as_str() == Some(uid.as_str())
        && original_request_metadata_matches(
            ticket,
            nonce,
            metadata["name"].as_str(),
            metadata["namespace"].as_str(),
            metadata["annotations"][ISOLATION_ID_ANNOTATION].as_str(),
            metadata["annotations"][REQUEST_DIGEST_ANNOTATION].as_str(),
            metadata["deletionTimestamp"].is_null(),
        )
}

/// Only these exact collection POSTs can cross the broker. Neither an exec
/// subresource nor a foreign namespace nor an arbitrary path can borrow the
/// Coordinator's Kubernetes credential.
fn resource_for(path: &str, namespace: &str) -> Option<&'static str> {
    match path.split('/').collect::<Vec<_>>().as_slice() {
        ["", "api", "v1", "namespaces", actual, "pods"] if *actual == namespace => Some("pods"),
        ["", "api", "v1", "namespaces", actual, "configmaps"] if *actual == namespace => {
            Some("configmaps")
        }
        ["", "api", "v1", "namespaces", actual, "secrets"] if *actual == namespace => {
            Some("secrets")
        }
        [
            "",
            "api",
            "v1",
            "namespaces",
            actual,
            "persistentvolumeclaims",
        ] if *actual == namespace => Some("persistentvolumeclaims"),
        ["", "apis", "batch", "v1", "namespaces", actual, "jobs"] if *actual == namespace => {
            Some("jobs")
        }
        ["", "apis", "apps", "v1", "namespaces", actual, "daemonsets"] if *actual == namespace => {
            Some("daemonsets")
        }
        _ => None,
    }
}

fn group_for(worker: &WorkerIdentity) -> String {
    collision_resistant_fingerprint(
        "k8s-worker-create-group-v1",
        &[
            worker.worker_id.as_bytes(),
            &worker.generation.to_be_bytes(),
            worker.incarnation_id.as_bytes(),
        ],
    )
}

fn validate_create(
    worker: &WorkerIdentity,
    namespace: &str,
    create: K8sCreateRequest,
) -> Result<ValidatedCreate, ApplicationError> {
    let resource = resource_for(&create.path, namespace).ok_or_else(|| {
        ApplicationError::invalid("Kubernetes Create path is outside the Worker Sandbox namespace")
    })?;
    let metadata = create
        .object
        .get("metadata")
        .and_then(Value::as_object)
        .ok_or_else(|| ApplicationError::invalid("Kubernetes Create object has no metadata"))?;
    let name = metadata
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| {
            !name.is_empty() && name.len() <= 253 && name.trim() == *name && !name.contains('/')
        })
        .ok_or_else(|| ApplicationError::invalid("Kubernetes Create object has no exact name"))?;
    if metadata.get("generateName").is_some()
        || metadata
            .get("namespace")
            .and_then(Value::as_str)
            .is_some_and(|value| value != namespace)
    {
        return Err(ApplicationError::invalid(
            "Kubernetes Create object names another namespace or generated identity",
        ));
    }
    let realization_digest = metadata
        .get("annotations")
        .and_then(|annotations| annotations.get(REALIZATION_DIGEST))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            ApplicationError::invalid(
                "Kubernetes Create object lacks its immutable realization digest",
            )
        })?;
    if realization_digest.len() != 64
        || !realization_digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(ApplicationError::invalid(
            "Kubernetes Create realization digest is malformed",
        ));
    }
    if metadata
        .get("labels")
        .and_then(Value::as_object)
        .is_some_and(|labels| labels.contains_key(ISOLATION_LABEL))
        || metadata
            .get("annotations")
            .and_then(Value::as_object)
            .is_some_and(|annotations| {
                annotations.contains_key(ISOLATION_ID_ANNOTATION)
                    || annotations.contains_key(REQUEST_DIGEST_ANNOTATION)
            })
        || metadata.get("finalizers").is_some_and(|finalizers| {
            finalizers.as_array().is_none_or(|values| {
                values.iter().any(|value| {
                    matches!(
                        value.as_str(),
                        Some(ORIGINAL_GUARD_FINALIZER | ISOLATION_LABEL)
                    )
                })
            })
        })
    {
        return Err(ApplicationError::invalid(
            "Worker Create cannot supply provider-owned isolation evidence",
        ));
    }
    let body = serde_json::to_vec(&create.object)
        .map_err(|_| ApplicationError::invalid("Kubernetes Create object cannot be encoded"))?;
    let fingerprint =
        collision_resistant_fingerprint("k8s-create-request-v1", &[create.path.as_bytes(), &body]);
    let digest = collision_resistant_fingerprint_digest(&fingerprint)
        .expect("canonical fingerprint has a digest");
    let ticket = CreateTicketRequest::new(group_for(worker), resource, namespace, name, digest)
        .map_err(|_| ApplicationError::invalid("Kubernetes Create ticket identity is invalid"))?;
    Ok(ValidatedCreate {
        path: create.path,
        body,
        ticket,
    })
}

fn stamp_admitted_create(
    body: Vec<u8>,
    resource: &str,
    isolation_id: &str,
    request_digest: &str,
) -> Result<Vec<u8>, ApplicationError> {
    if isolation_id.is_empty() {
        return Err(ApplicationError::unavailable(
            "historical Create has no provider-owned isolation identity",
        ));
    }
    let mut object: Value = serde_json::from_slice(&body)
        .map_err(|_| ApplicationError::internal("admitted Create bytes are invalid"))?;
    let metadata = object
        .get_mut("metadata")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| ApplicationError::internal("admitted Create metadata is missing"))?;
    let annotations = metadata.entry("annotations").or_insert_with(|| json!({}));
    let annotations = annotations
        .as_object_mut()
        .ok_or_else(|| ApplicationError::internal("admitted Create annotations are invalid"))?;
    annotations.insert(ISOLATION_ID_ANNOTATION.into(), isolation_id.into());
    annotations.insert(REQUEST_DIGEST_ANNOTATION.into(), request_digest.into());
    // Only the Pod disposal owner currently releases the original-result
    // guard. Other Kinds retain exact identity annotations but cannot gain a
    // finalizer that their ordinary lifecycle has no authority to remove.
    if resource == "pods" {
        let finalizers = metadata
            .entry("finalizers")
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or_else(|| ApplicationError::internal("admitted Create finalizers are invalid"))?;
        finalizers.push(ORIGINAL_GUARD_FINALIZER.into());
    }
    serde_json::to_vec(&object)
        .map_err(|_| ApplicationError::internal("admitted Create cannot be encoded"))
}

fn binding<'a>(
    ledger: &K8sWorkerCreateLedger,
    anchor: &'a WorkerCreateLedger,
    pod: &'a KubernetesPodIdentity,
    provider_id: &str,
) -> Result<K8sLedgerBinding<'a>, ApplicationError> {
    if anchor.provider_id() != provider_id
        || anchor.namespace() != ledger.control_namespace()
        || anchor.configuration_fingerprint() != ledger.configuration_fingerprint()
    {
        return Err(ApplicationError::unavailable(
            "Worker Create ledger provider configuration changed",
        ));
    }
    Ok(K8sLedgerBinding {
        worker_namespace: pod.namespace(),
        worker_name: pod.name(),
        worker_uid: pod.uid(),
        ledger_name: anchor.name(),
        ledger_uid: anchor.uid(),
        configuration_fingerprint: anchor.configuration_fingerprint(),
    })
}

/// One original-anchor seal/join used by terminal recovery and both Worker
/// evidence-release purposes. Complete admission fencing is not disposal.
pub(super) async fn seal_worker_requests<'a>(
    ledger: &K8sWorkerCreateLedger,
    anchor: &'a WorkerCreateLedger,
    pod: &'a KubernetesPodIdentity,
    worker: &WorkerIdentity,
    provider_id: &str,
) -> Result<(K8sLedgerBinding<'a>, RequestJoin), ApplicationError> {
    if !pod.matches_worker(worker) {
        return Err(ApplicationError::invalid(
            "Worker Create seal has another incarnation",
        ));
    }
    let exact = binding(ledger, anchor, pod, provider_id)?;
    ledger.seal_worker(exact).await.map_err(|_| {
        ApplicationError::unavailable("old Worker Create ledger could not be sealed")
    })?;
    let joined = ledger
        .seal_group_and_join(exact, &group_for(worker))
        .await
        .map_err(|_| ApplicationError::unavailable("old Worker Create requests remain unjoined"))?;
    Ok((exact, joined))
}

async fn verify_request(
    state: &CreateState,
    worker: &VerifiedWorkerContext,
    identity: &WorkerIdentity,
    require_ready: bool,
) -> Result<(), ApplicationError> {
    verify_current_worker_identity(
        state.directory.as_ref(),
        worker,
        identity,
        state.clock.now_ms(),
        require_ready,
    )
    .await
    .map_err(|_| {
        ApplicationError::invalid(if require_ready {
            "Worker Create identity is not current and Ready"
        } else {
            "Worker Seal identity is not current and live"
        })
    })?;
    Ok(())
}

impl K8sCreateBroker {
    pub(super) fn new(
        api_client: Client,
        ledger: K8sWorkerCreateLedger,
        sandbox_namespace: &str,
        provider_id: &str,
    ) -> Self {
        Self {
            api_client,
            ledger,
            sandbox_namespace: sandbox_namespace.into(),
            provider_id: provider_id.into(),
        }
    }

    pub(in crate::worker_transport) fn router(
        self: Arc<Self>,
        directory: Arc<dyn WorkerDirectory>,
        authenticator: Arc<dyn WorkerRequestAuthenticator>,
        clock: Arc<dyn WallClock>,
    ) -> Router {
        async fn create(
            State(state): State<CreateState>,
            Extension(worker): Extension<VerifiedWorkerContext>,
            Json(request): Json<CreateWire>,
        ) -> AxumResponse {
            let result = async {
                if request.wire_version != K8S_CREATE_WIRE_VERSION {
                    return Err(ApplicationError::invalid(
                        "unsupported Create control wire version",
                    ));
                }
                verify_request(&state, &worker, &request.identity, true).await?;
                let response = state
                    .broker
                    .create(
                        state.directory.clone(),
                        state.clock.clone(),
                        request.identity,
                        request.create,
                    )
                    .await?;
                Ok(response)
            }
            .await;
            match result {
                Ok(response) => response.into_http_response(),
                Err(error) => {
                    awaken_dispatch_http::worker_transport_response(Err(error)).into_response()
                }
            }
        }

        async fn seal(
            State(state): State<CreateState>,
            Extension(worker): Extension<VerifiedWorkerContext>,
            Json(request): Json<SealWire>,
        ) -> AxumResponse {
            let result = async {
                if request.wire_version != K8S_CREATE_WIRE_VERSION {
                    return Err(ApplicationError::invalid(
                        "unsupported Create control wire version",
                    ));
                }
                verify_request(&state, &worker, &request.identity, false).await?;
                let joined = state
                    .broker
                    .seal_predecessor(
                        state.directory.clone(),
                        &request.scope,
                        &request.predecessor,
                        &request.expected_pod_name,
                    )
                    .await?;
                let bytes = CreateReplyProfile::WholeJoin
                    .encode_body(&joined)
                    .map_err(|_| {
                        ApplicationError::unavailable(
                            "complete Create join response exceeds its capture policy",
                        )
                    })?;
                Ok(BrokerCreateReply::from_encoded(
                    bytes,
                    CreateReplyProfile::WholeJoin,
                ))
            }
            .await;
            match result {
                Ok(response) => response.into_http_response(),
                Err(error) => {
                    awaken_dispatch_http::worker_transport_response(Err(error)).into_response()
                }
            }
        }

        let state = CreateState {
            broker: self,
            directory,
            clock,
        };
        Router::new()
            .route(PATH, post(create))
            .route(SEAL_PATH, post(seal))
            .route_layer(axum::middleware::from_fn_with_state(
                authenticator,
                authenticate_worker_request,
            ))
            .with_state(state)
    }

    async fn create(
        &self,
        directory: Arc<dyn WorkerDirectory>,
        clock: Arc<dyn WallClock>,
        identity: WorkerIdentity,
        create: K8sCreateRequest,
    ) -> Result<BrokerCreateReply, ApplicationError> {
        let pod = KubernetesPodIdentity::from_worker_id(&identity.worker_id)
            .map_err(ApplicationError::invalid)?;
        if pod.namespace() != self.sandbox_namespace || !pod.matches_worker(&identity) {
            return Err(ApplicationError::invalid(
                "Worker Create names another physical Pod",
            ));
        }
        let record = directory
            .current(&identity.worker_id)
            .await
            .map_err(|_| ApplicationError::unavailable("Worker Create Registry read unavailable"))?
            .ok_or_else(|| {
                ApplicationError::unavailable("Worker Create Registry record missing")
            })?;
        if !current_create_worker(&record, &identity, None, clock.now_ms()) {
            return Err(ApplicationError::invalid(
                "Worker Create incarnation is not current and Ready",
            ));
        }
        let anchor = record.create_ledger.ok_or_else(|| {
            ApplicationError::unavailable("Worker Create has no frozen physical ledger")
        })?;
        let ledger_binding = binding(&self.ledger, &anchor, &pod, &self.provider_id)?;
        let validated = validate_create(&identity, &self.sandbox_namespace, create)?;
        let (admission, profile) = self
            .ledger
            .admit_request(ledger_binding, &validated.ticket)
            .await
            .map_err(|_| {
                ApplicationError::unavailable("Worker Create ledger admission unavailable")
            })?;
        match admission {
            TicketAdmission::Forward => {
                let (sender, receiver) = tokio::sync::oneshot::channel();
                let broker = self.clone();
                tokio::spawn(async move {
                    let result = broker
                        .forward_and_record(
                            directory,
                            clock,
                            identity,
                            pod,
                            anchor,
                            AdmittedCreate {
                                create: validated,
                                profile,
                            },
                        )
                        .await;
                    let _ = sender.send(result);
                });
                receiver.await.map_err(|_| {
                    ApplicationError::unavailable(
                        "Worker Create executor stopped before a durable result",
                    )
                })?
            }
            TicketAdmission::InFlight => Err(ApplicationError::unavailable(
                "Worker Create request remains InFlight",
            )),
            TicketAdmission::Finished(result @ TicketResult::Created { .. }) => {
                let ticket = self
                    .ledger
                    .ticket_for(ledger_binding, &validated.ticket)
                    .await
                    .map_err(|_| {
                        ApplicationError::unavailable("original Create ticket readback unavailable")
                    })?
                    .ok_or_else(|| {
                        ApplicationError::unavailable("original Create ticket missing")
                    })?;
                if ticket.isolation_id.is_empty()
                    || ticket.isolation_pending
                    || !matches!(
                        &ticket.status,
                        awaken_sandbox_container::k8s::TicketStatus::Finished(existing)
                            if existing == &result
                    )
                {
                    return Err(ApplicationError::unavailable(
                        "original Create result is not joined",
                    ));
                }
                let object = self
                    .read_original_result(&validated, &result, &ticket.isolation_id, profile)
                    .await?;
                let current = directory.current(&identity.worker_id).await.map_err(|_| {
                    ApplicationError::unavailable("original Create Registry readback unavailable")
                })?;
                if !current.as_ref().is_some_and(|record| {
                    current_create_worker(record, &identity, Some(&anchor), clock.now_ms())
                }) {
                    return Err(ApplicationError::unavailable(
                        "original Create Worker authority changed during readback",
                    ));
                }
                // Same endpoint, original object, no second POST and no new UID
                // claim. The typed API caller can now retain the original result
                // instead of treating a later generic 409+GET as that receipt.
                BrokerCreateReply::new(
                    K8sCreateResponse {
                        status: 200,
                        object,
                    },
                    profile,
                )
            }
            TicketAdmission::Finished(TicketResult::Isolated { .. }) => Err(
                ApplicationError::unavailable("isolated Create cannot replay an original object"),
            ),
            TicketAdmission::Finished(TicketResult::RejectedNoEffect { code }) => {
                BrokerCreateReply::new(status_response(code, "Rejected"), profile)
            }
            TicketAdmission::Sealed => Err(ApplicationError::unavailable(
                "Worker Create request group is sealed",
            )),
        }
    }

    async fn read_original_result(
        &self,
        validated: &ValidatedCreate,
        result: &TicketResult,
        nonce: &str,
        profile: CreateReplyProfile,
    ) -> Result<Value, ApplicationError> {
        let request = Request::builder()
            .method(Method::GET)
            .uri(format!("{}/{}", validated.path, validated.ticket.name))
            .body(Body::empty())
            .map_err(|_| ApplicationError::internal("original Create readback path invalid"))?;
        let response = self.api_client.send(request).await.map_err(|_| {
            ApplicationError::unavailable("original Create API readback unavailable")
        })?;
        if response.status() != StatusCode::OK {
            return Err(ApplicationError::unavailable(
                "original Create object is no longer observable",
            ));
        }
        let bytes = collect_create_body(response.into_body(), profile)
            .await
            .map_err(|_| {
                ApplicationError::unavailable(
                    "original Create API readback incomplete or outside capture policy",
                )
            })?;
        let object: Value = serde_json::from_slice(&bytes).map_err(|_| {
            ApplicationError::unavailable("original Create API readback undecodable")
        })?;
        if !original_readback_matches(&object, &validated.ticket, result, nonce) {
            return Err(ApplicationError::unavailable(
                "original Create API readback differs from its retained ticket",
            ));
        }
        Ok(object)
    }

    /// Close Create admission through its original provider owner after the
    /// exact Registry incarnation closes. The ledger CAS and ticket join, not
    /// process exit, fence late Creates. A merely expired Ready Worker or absent
    /// original anchor stays Unknown. This grants no physical-exit or disposal
    /// evidence for the Worker or any existing participant.
    async fn seal_predecessor(
        &self,
        directory: Arc<dyn WorkerDirectory>,
        scope: &str,
        predecessor: &ContainerEffectFence,
        expected_pod_name: &str,
    ) -> Result<RequestJoin, ApplicationError> {
        if scope.trim().is_empty() || scope.trim() != scope || scope.len() > 256 {
            return Err(ApplicationError::invalid("Sandbox Create scope is invalid"));
        }
        let Some(identity) = WorkerIdentity::from_lease_owner(
            predecessor.owner(),
            predecessor.runtime_incarnation(),
        ) else {
            return Ok(RequestJoin::Unknown);
        };
        let pod = KubernetesPodIdentity::from_worker_id(&identity.worker_id)
            .map_err(ApplicationError::invalid)?;
        if pod.namespace() != self.sandbox_namespace || !pod.matches_worker(&identity) {
            return Ok(RequestJoin::Unknown);
        }
        let Some(record) = directory.incarnation(&identity).await.map_err(|_| {
            ApplicationError::unavailable("historical Worker Create Registry read unavailable")
        })?
        else {
            return Ok(RequestJoin::Unknown);
        };
        if !matches!(
            record.snapshot.state,
            WorkerState::Quiesced | WorkerState::Dead
        ) {
            return Ok(RequestJoin::Unknown);
        }
        let Some(anchor) = record.create_ledger else {
            return Ok(RequestJoin::Unknown);
        };
        let (exact, _) =
            seal_worker_requests(&self.ledger, &anchor, &pod, &identity, &self.provider_id).await?;
        let group = group_for(&identity);
        self.ledger
            .reserve_terminal_claim_pod_and_join(exact, &group, scope, expected_pod_name)
            .await
            .map_err(|_| ApplicationError::unavailable("old Worker Create isolation is unverified"))
    }

    async fn forward_and_record(
        &self,
        directory: Arc<dyn WorkerDirectory>,
        clock: Arc<dyn WallClock>,
        identity: WorkerIdentity,
        pod: KubernetesPodIdentity,
        anchor: WorkerCreateLedger,
        create: AdmittedCreate,
    ) -> Result<BrokerCreateReply, ApplicationError> {
        let AdmittedCreate { create, profile } = create;
        let current = directory.current(&identity.worker_id).await.map_err(|_| {
            ApplicationError::unavailable("Worker Create Registry recheck unavailable")
        })?;
        if !current.as_ref().is_some_and(|record| {
            current_create_worker(record, &identity, Some(&anchor), clock.now_ms())
        }) {
            let result = TicketResult::RejectedNoEffect { code: 403 };
            let reply = BrokerCreateReply::new(status_response(403, "Forbidden"), profile)?;
            self.ledger
                .finish_request(
                    binding(&self.ledger, &anchor, &pod, &self.provider_id)?,
                    &create.ticket,
                    &result,
                )
                .await
                .map_err(|_| {
                    ApplicationError::unavailable("Worker Create refusal could not be retained")
                })?;
            return Ok(reply);
        }
        let admitted = self
            .ledger
            .ticket_for(
                binding(&self.ledger, &anchor, &pod, &self.provider_id)?,
                &create.ticket,
            )
            .await
            .map_err(|_| ApplicationError::unavailable("admitted Create ticket is unavailable"))?
            .ok_or_else(|| ApplicationError::unavailable("admitted Create ticket is missing"))?;
        let body = stamp_admitted_create(
            create.body,
            &create.ticket.resource,
            &admitted.isolation_id,
            &create.ticket.digest,
        )?;
        let request = Request::builder()
            .method(Method::POST)
            .uri(create.path.as_str())
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .map_err(|_| ApplicationError::internal("Worker Create path could not be encoded"))?;
        let response =
            self.api_client.send(request).await.map_err(|_| {
                ApplicationError::unavailable("Worker Create API outcome is unknown")
            })?;
        let status = response.status().as_u16();
        let bytes = collect_create_body(response.into_body(), profile)
            .await
            .map_err(|_| {
                ApplicationError::unavailable(
                    "Worker Create API response is incomplete or outside capture policy",
                )
            })?;
        let object: Value = serde_json::from_slice(&bytes).map_err(|_| {
            ApplicationError::unavailable("Worker Create API response is undecodable")
        })?;
        let result = if status == 201 {
            let metadata = object
                .get("metadata")
                .and_then(Value::as_object)
                .ok_or_else(|| {
                    ApplicationError::unavailable("Worker Create API result has no metadata")
                })?;
            if metadata.get("name").and_then(Value::as_str) != Some(create.ticket.name.as_str())
                || metadata.get("namespace").and_then(Value::as_str)
                    != Some(create.ticket.namespace.as_str())
            {
                return Err(ApplicationError::unavailable(
                    "Worker Create API result names another object",
                ));
            }
            let uid = metadata.get("uid").and_then(Value::as_str).ok_or_else(|| {
                ApplicationError::unavailable("Worker Create API result lacks UID")
            })?;
            let result = TicketResult::Created { uid: uid.into() };
            result
                .validate()
                .map_err(|_| ApplicationError::unavailable("Worker Create API result lacks UID"))?;
            if !original_readback_matches(&object, &create.ticket, &result, &admitted.isolation_id)
            {
                return Err(ApplicationError::unavailable(
                    "Worker Create API result changed its admitted identity",
                ));
            }
            result
        } else {
            let result = TicketResult::RejectedNoEffect { code: status };
            result.validate().map_err(|_| {
                ApplicationError::unavailable("Worker Create API outcome is not definitive")
            })?;
            result
        };
        // Fresh raw API and the exact entire outgoing reply must both fit
        // before a definitive result is retained. Oversize stays InFlight.
        let reply = BrokerCreateReply::new(K8sCreateResponse { status, object }, profile)?;
        self.ledger
            .finish_request(
                binding(&self.ledger, &anchor, &pod, &self.provider_id)?,
                &create.ticket,
                &result,
            )
            .await
            .map_err(|_| {
                ApplicationError::unavailable("Worker Create API outcome could not be retained")
            })?;
        Ok(reply)
    }
}

#[cfg(test)]
mod tests;
