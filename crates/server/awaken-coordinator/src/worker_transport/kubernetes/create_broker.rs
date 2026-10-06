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
    ApplicationError, WorkerCreateLedger, WorkerDirectory, WorkerIdentity, WorkerState,
};
use awaken_sandbox_container::ContainerEffectFence;
use awaken_sandbox_container::k8s::{
    CreateTicketRequest, ISOLATION_ID_ANNOTATION, ISOLATION_LABEL, K8sCreateRequest,
    K8sCreateResponse, K8sLedgerBinding, K8sWorkerCreateLedger, ORIGINAL_GUARD_FINALIZER,
    PodProcessBinding, REQUEST_DIGEST_ANNOTATION, RequestJoin, TicketAdmission, TicketResult,
};
use awaken_time::{WallClock, WallClockExt};
use awaken_worker_transport_security::kubernetes::KubernetesPodIdentity;
use awaken_worker_transport_security::{
    VerifiedWorkerContext, WorkerRequestAuthenticator, authenticate_worker_request,
    verify_current_worker_identity,
};
use axum::extract::{Extension, State};
use axum::http::{Method, Request, StatusCode, header};
use axum::routing::post;
use axum::{Json, Router};
use kube::Client;
use kube::client::Body;
use serde::Deserialize;
use serde_json::{Value, json};

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

#[derive(Clone)]
struct CreateState {
    broker: Arc<K8sCreateBroker>,
    directory: Arc<dyn WorkerDirectory>,
    clock: Arc<dyn WallClock>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateWire {
    identity: WorkerIdentity,
    create: K8sCreateRequest,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SealWire {
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

async fn verify_request(
    state: &CreateState,
    worker: &VerifiedWorkerContext,
    identity: &WorkerIdentity,
) -> Result<(), ApplicationError> {
    verify_current_worker_identity(
        state.directory.as_ref(),
        worker,
        identity,
        state.clock.now_ms(),
        true,
    )
    .await
    .map_err(|_| ApplicationError::invalid("Worker Create identity is not current and Ready"))?;
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
        ) -> (StatusCode, Json<Value>) {
            let result = async {
                verify_request(&state, &worker, &request.identity).await?;
                let response = state
                    .broker
                    .create(
                        state.directory.clone(),
                        state.clock.clone(),
                        request.identity,
                        request.create,
                    )
                    .await?;
                serde_json::to_value(response).map_err(|_| {
                    ApplicationError::internal("Worker Create response encoding failed")
                })
            }
            .await;
            awaken_dispatch_http::worker_transport_response(result)
        }

        async fn seal(
            State(state): State<CreateState>,
            Extension(worker): Extension<VerifiedWorkerContext>,
            Json(request): Json<SealWire>,
        ) -> (StatusCode, Json<Value>) {
            let result = async {
                verify_request(&state, &worker, &request.identity).await?;
                let joined = state
                    .broker
                    .seal_predecessor(
                        state.directory.clone(),
                        &request.scope,
                        &request.predecessor,
                        &request.expected_pod_name,
                    )
                    .await?;
                serde_json::to_value(joined).map_err(|_| {
                    ApplicationError::internal("Worker Create join response encoding failed")
                })
            }
            .await;
            awaken_dispatch_http::worker_transport_response(result)
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
    ) -> Result<K8sCreateResponse, ApplicationError> {
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
        if record.snapshot.identity != identity
            || record.snapshot.state != WorkerState::Ready
            || record.snapshot.expires_at_ms <= clock.now_ms()
        {
            return Err(ApplicationError::invalid(
                "Worker Create incarnation is not current and Ready",
            ));
        }
        let anchor = record.create_ledger.ok_or_else(|| {
            ApplicationError::unavailable("Worker Create has no frozen physical ledger")
        })?;
        let ledger_binding = binding(&self.ledger, &anchor, &pod, &self.provider_id)?;
        let validated = validate_create(&identity, &self.sandbox_namespace, create)?;
        let admission = self
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
                        .forward_and_record(directory, clock, identity, pod, anchor, validated)
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
            TicketAdmission::Finished(TicketResult::Created { .. }) => {
                Ok(status_response(409, "AlreadyExists"))
            }
            TicketAdmission::Finished(TicketResult::Isolated { .. }) => {
                Ok(status_response(409, "AlreadyExists"))
            }
            TicketAdmission::Finished(TicketResult::RejectedNoEffect { code }) => {
                Ok(status_response(code, "Rejected"))
            }
            TicketAdmission::Sealed => Err(ApplicationError::unavailable(
                "Worker Create request group is sealed",
            )),
        }
    }

    /// Reuse the Registry's already-committed exact process-exit receipt. A
    /// Worker that is merely expired, a missing historical record or a Pod
    /// coordinate that cannot be decoded never authorizes ledger closure.
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
        let Some(exit) = record.process_exit else {
            return Ok(RequestJoin::Unknown);
        };
        let process: PodProcessBinding =
            serde_json::from_str(exit.execution_ref()).map_err(|_| {
                ApplicationError::unavailable("historical Worker process binding is undecodable")
            })?;
        if exit.provider_id() != self.provider_id
            || !process.matches_pod(pod.namespace(), pod.name(), pod.uid())
            || !process.worker_evidence_was_retained()
        {
            return Ok(RequestJoin::Unknown);
        }
        let Some(anchor) = record.create_ledger else {
            return Ok(RequestJoin::Unknown);
        };
        let exact = binding(&self.ledger, &anchor, &pod, &self.provider_id)?;
        self.ledger.seal_worker(exact).await.map_err(|_| {
            ApplicationError::unavailable("old Worker Create ledger could not be sealed")
        })?;
        let group = group_for(&identity);
        self.ledger
            .seal_group_and_join(exact, &group)
            .await
            .map_err(|_| {
                ApplicationError::unavailable("old Worker Create requests remain unjoined")
            })?;
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
        create: ValidatedCreate,
    ) -> Result<K8sCreateResponse, ApplicationError> {
        let current = directory.current(&identity.worker_id).await.map_err(|_| {
            ApplicationError::unavailable("Worker Create Registry recheck unavailable")
        })?;
        if !current.is_some_and(|record| {
            record.snapshot.identity == identity
                && record.snapshot.state == WorkerState::Ready
                && record.snapshot.expires_at_ms > clock.now_ms()
                && record.create_ledger.as_ref() == Some(&anchor)
        }) {
            let result = TicketResult::RejectedNoEffect { code: 403 };
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
            return Ok(status_response(403, "Forbidden"));
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
        let bytes = response.into_body().collect_bytes().await.map_err(|_| {
            ApplicationError::unavailable("Worker Create API response is incomplete")
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
            let uid = metadata
                .get("uid")
                .and_then(Value::as_str)
                .filter(|uid| !uid.trim().is_empty() && uid.trim() == *uid)
                .ok_or_else(|| {
                    ApplicationError::unavailable("Worker Create API result lacks UID")
                })?;
            TicketResult::Created { uid: uid.into() }
        } else if matches!(status, 400 | 401 | 403 | 409 | 422) {
            TicketResult::RejectedNoEffect { code: status }
        } else {
            return Err(ApplicationError::unavailable(
                "Worker Create API outcome is not definitive",
            ));
        };
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
        Ok(K8sCreateResponse { status, object })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_lease_testkit::ManualClock;
    use awaken_dispatch::{WorkerHeartbeat, WorkerManifest, WorkerRegistration};
    use awaken_worker_registry::MemoryWorkerDirectory;
    use awaken_worker_transport_security::{HeaderWorkerAuthenticator, WORKER_ID_HEADER};
    use axum::body::Body as AxumBody;
    use axum::http::Response;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Notify;
    use tower::ServiceExt as _;

    #[test]
    fn create_path_and_object_admission_are_total_and_namespace_bound() {
        // Causes: C1 collection resource is one of Pod/PVC/ConfigMap/Secret/
        // Job/DaemonSet; C2 namespace exact; C3 no subresource or query;
        // C4 deterministic name and stamped digest. Effects: E1 only the full
        // tuple yields one ticket; E2 a one-axis mutation is rejected before
        // any provider CAS or Kubernetes API effect.
        let identity = WorkerIdentity::new("k8s:ns:worker:uid", "uid", 1);
        let object = json!({"metadata":{
            "name":"target", "namespace":"ns",
            "annotations": {"awaken.dev/realization-digest": "a".repeat(64)}
        }});
        for path in [
            "/api/v1/namespaces/ns/pods",
            "/api/v1/namespaces/ns/configmaps",
            "/api/v1/namespaces/ns/secrets",
            "/api/v1/namespaces/ns/persistentvolumeclaims",
            "/apis/batch/v1/namespaces/ns/jobs",
            "/apis/apps/v1/namespaces/ns/daemonsets",
        ] {
            assert!(
                validate_create(
                    &identity,
                    "ns",
                    K8sCreateRequest {
                        path: path.into(),
                        object: object.clone()
                    }
                )
                .is_ok(),
                "E1 {path}"
            );
        }
        for path in [
            "/api/v1/namespaces/foreign/pods",
            "/api/v1/namespaces/ns/pods/target/exec",
            "/api/v1/namespaces/ns/pods?dryRun=All",
            "/apis/batch/v1/namespaces/ns/cronjobs",
        ] {
            assert!(
                validate_create(
                    &identity,
                    "ns",
                    K8sCreateRequest {
                        path: path.into(),
                        object: object.clone()
                    }
                )
                .is_err(),
                "E2 {path}"
            );
        }
        for mutation in [
            json!({"metadata":{"name":"target","namespace":"foreign","annotations":{"awaken.dev/realization-digest":"a".repeat(64)}}}),
            json!({"metadata":{"name":"target","namespace":"ns","generateName":"other-","annotations":{"awaken.dev/realization-digest":"a".repeat(64)}}}),
            json!({"metadata":{"name":"target","namespace":"ns"}}),
            json!({"metadata":{"name":"target","namespace":"ns","labels":{(ISOLATION_LABEL):"true"},"annotations":{"awaken.dev/realization-digest":"a".repeat(64)}}}),
            json!({"metadata":{"name":"target","namespace":"ns","annotations":{"awaken.dev/realization-digest":"a".repeat(64),(ISOLATION_ID_ANNOTATION):"forged"}}}),
            json!({"metadata":{"name":"target","namespace":"ns","finalizers":[ORIGINAL_GUARD_FINALIZER],"annotations":{"awaken.dev/realization-digest":"a".repeat(64)}}}),
        ] {
            assert!(
                validate_create(
                    &identity,
                    "ns",
                    K8sCreateRequest {
                        path: "/api/v1/namespaces/ns/pods".into(),
                        object: mutation
                    }
                )
                .is_err(),
                "E2 object axis"
            );
        }
    }

    #[test]
    fn provider_isolation_metadata_is_reserved_on_every_brokered_kind() {
        // Causes: C1 each of the six admitted Kubernetes collections; C2
        // provider-owned evidence is absent, or the Worker supplies the
        // reservation label, isolation annotation, or either provider
        // finalizer. Constraint: exact namespace/name and realization digest
        // are otherwise valid. Effects: E1 absent C2 admits one ticket; E2
        // every forged C2 is refused before ledger CAS or API I/O.
        // Decision rows M1=C1+absent->E1; M2-M4=C1+each forged axis->E2.
        let identity = WorkerIdentity::new("k8s:ns:worker:uid", "uid", 1);
        for path in [
            "/api/v1/namespaces/ns/pods",
            "/api/v1/namespaces/ns/persistentvolumeclaims",
            "/api/v1/namespaces/ns/configmaps",
            "/api/v1/namespaces/ns/secrets",
            "/apis/batch/v1/namespaces/ns/jobs",
            "/apis/apps/v1/namespaces/ns/daemonsets",
        ] {
            let base = json!({"metadata":{
                "name":"target", "namespace":"ns",
                "annotations":{"awaken.dev/realization-digest":"a".repeat(64)}
            }});
            let request = |object| K8sCreateRequest {
                path: path.into(),
                object,
            };
            assert!(
                validate_create(&identity, "ns", request(base.clone())).is_ok(),
                "M1 {path}"
            );
            let mut label = base.clone();
            label["metadata"]["labels"] = json!({(ISOLATION_LABEL):"true"});
            let mut annotation = base.clone();
            annotation["metadata"]["annotations"][ISOLATION_ID_ANNOTATION] = "forged".into();
            let mut finalizer = base;
            finalizer["metadata"]["finalizers"] = json!([ORIGINAL_GUARD_FINALIZER]);
            let mut reservation_finalizer = finalizer.clone();
            reservation_finalizer["metadata"]["finalizers"] = json!([ISOLATION_LABEL]);
            for (axis, object) in [
                ("label", label),
                ("annotation", annotation),
                ("finalizer", finalizer),
                ("reservation finalizer", reservation_finalizer),
            ] {
                assert!(
                    validate_create(&identity, "ns", request(object)).is_err(),
                    "M2-M4 {path} {axis}"
                );
            }
        }
    }

    #[test]
    fn broker_stamps_identity_on_every_kind_and_guards_only_pod() {
        // Causes: C1 one of six brokered Kind paths has won the durable CAS;
        // C2 its ticket has a provider nonce or a historical missing nonce;
        // C3 the Worker supplies ordinary metadata and no protected markers.
        // Effects: E1 C1+C2(valid)+C3 adds the exact nonce/digest without
        // changing the Worker projection; E2 only Pod gains the guard its
        // existing disposal owner can release; E3 C2(missing) refuses
        // forwarding before an API call. Decision rules B1=valid+C3->E1,
        // B2=B1+Pod->E2, B3=missing->E3.
        let body = serde_json::to_vec(&json!({
            "metadata":{"name":"target","annotations":{"awaken.dev/realization-digest":"a".repeat(64)}},
            "spec":{"keep":"worker"}
        }))
        .unwrap();
        for resource in [
            "pods",
            "persistentvolumeclaims",
            "configmaps",
            "secrets",
            "jobs",
            "daemonsets",
        ] {
            let stamped = stamp_admitted_create(
                body.clone(),
                resource,
                "0123456789abcdef0123456789abcdef",
                &"b".repeat(64),
            )
            .unwrap();
            let value: Value = serde_json::from_slice(&stamped).unwrap();
            assert_eq!(value["spec"]["keep"], "worker", "B1 {resource}");
            assert_eq!(
                value["metadata"]["annotations"][ISOLATION_ID_ANNOTATION],
                "0123456789abcdef0123456789abcdef",
                "B1 {resource}"
            );
            assert_eq!(
                value["metadata"]["annotations"][REQUEST_DIGEST_ANNOTATION],
                "b".repeat(64),
                "B1 {resource}"
            );
            assert_eq!(
                value["metadata"]["finalizers"],
                if resource == "pods" {
                    json!([ORIGINAL_GUARD_FINALIZER])
                } else {
                    Value::Null
                },
                "B2 {resource}"
            );
            assert!(
                stamp_admitted_create(body.clone(), resource, "", &"b".repeat(64)).is_err(),
                "B3 {resource}"
            );
        }
    }

    #[tokio::test]
    async fn worker_response_loss_does_not_cancel_the_one_admitted_create() {
        // Causes: C1 exact Ready Worker and Registry-frozen ledger UID; C2 a
        // Pod Create has won ticket CAS but its API response is delayed; C3
        // Worker HTTP future disappears; C4 a retry arrives before/after the
        // independent executor durably records API UID. Effects: E1 one API
        // Create only, E2 early retry stays InFlight, E3 detached completion
        // survives C3, E4 late retry returns conflict for typed GET/verify.
        // Rules R1=C1+C2=>E1, R2=R1+C4(before)=>E2,
        // R3=R1+C3+API success=>E3, R4=R3+C4(after)=>E4. C5 a
        // Registry-persisted exact Pod exit arrives; E5 only C5 plus settled
        // tickets can seal and join, never an expired lease alone. C6 the
        // broker forwards a Pod after first CAS; E6 its two provider-owned
        // annotations equal the admitted ticket, and its original-Pod guard
        // finalizer is installed before Kubernetes can publish the Pod.
        let stored = Arc::new(Mutex::new(None::<Value>));
        let pod_posts = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let completed = Arc::new(Notify::new());
        let service = tower::service_fn({
            let (stored, pod_posts, started, release, completed) = (
                stored.clone(),
                pod_posts.clone(),
                started.clone(),
                release.clone(),
                completed.clone(),
            );
            move |request: Request<Body>| {
                let (stored, pod_posts, started, release, completed) = (
                    stored.clone(),
                    pod_posts.clone(),
                    started.clone(),
                    release.clone(),
                    completed.clone(),
                );
                async move {
                    let method = request.method().clone();
                    let path = request.uri().path().to_owned();
                    let bytes = request.into_body().collect_bytes().await.unwrap();
                    let body: Value = if bytes.is_empty() {
                        Value::Null
                    } else {
                        serde_json::from_slice(&bytes).unwrap()
                    };
                    let (code, value) = match (method.as_str(), path.as_str()) {
                        ("POST", "/api/v1/namespaces/control-ns/configmaps") => {
                            let mut created = body;
                            created["apiVersion"] = "v1".into();
                            created["kind"] = "ConfigMap".into();
                            created["metadata"]["uid"] = "ledger-uid".into();
                            created["metadata"]["resourceVersion"] = "1".into();
                            *stored.lock().unwrap() = Some(created.clone());
                            (201, created)
                        }
                        ("GET", path)
                            if path.starts_with("/api/v1/namespaces/control-ns/configmaps/") =>
                        {
                            (200, stored.lock().unwrap().clone().unwrap())
                        }
                        ("PUT", path)
                            if path.starts_with("/api/v1/namespaces/control-ns/configmaps/") =>
                        {
                            let mut current = stored.lock().unwrap();
                            let old = current.as_ref().unwrap();
                            if body["metadata"]["uid"] != old["metadata"]["uid"]
                                || body["metadata"]["resourceVersion"]
                                    != old["metadata"]["resourceVersion"]
                            {
                                (
                                    409,
                                    json!({"apiVersion":"v1","kind":"Status","status":"Failure","code":409,"reason":"Conflict"}),
                                )
                            } else {
                                let mut updated = body;
                                let next = old["metadata"]["resourceVersion"]
                                    .as_str()
                                    .unwrap()
                                    .parse::<u64>()
                                    .unwrap()
                                    + 1;
                                updated["metadata"]["resourceVersion"] = next.to_string().into();
                                let finished = updated["data"]["ledger"]
                                    .as_str()
                                    .unwrap()
                                    .contains("finished");
                                *current = Some(updated.clone());
                                if finished {
                                    completed.notify_one();
                                }
                                (200, updated)
                            }
                        }
                        ("POST", "/api/v1/namespaces/sandbox-ns/pods") => {
                            {
                                let stored = stored.lock().unwrap();
                                let ledger: Value = serde_json::from_str(
                                    stored.as_ref().unwrap()["data"]["ledger"].as_str().unwrap(),
                                )
                                .unwrap();
                                let ticket = ledger["tickets"]
                                    .as_object()
                                    .unwrap()
                                    .values()
                                    .next()
                                    .unwrap();
                                assert_eq!(
                                    body["metadata"]["annotations"][ISOLATION_ID_ANNOTATION],
                                    ticket["isolation_id"],
                                    "E6 exact admission identity"
                                );
                                assert_eq!(
                                    body["metadata"]["annotations"][REQUEST_DIGEST_ANNOTATION],
                                    ticket["request"]["digest"],
                                    "E6 exact request digest"
                                );
                                assert!(
                                    body["metadata"]["finalizers"]
                                        .as_array()
                                        .unwrap()
                                        .iter()
                                        .any(|value| value == ORIGINAL_GUARD_FINALIZER),
                                    "E6 original Pod cannot disappear before its ticket result"
                                );
                            }
                            pod_posts.fetch_add(1, Ordering::SeqCst);
                            started.notify_one();
                            release.notified().await;
                            (
                                201,
                                json!({"apiVersion":"v1","kind":"Pod","metadata":{
                                    "name":"sandbox-pod", "namespace":"sandbox-ns", "uid":"pod-uid"
                                }}),
                            )
                        }
                        _ => panic!("unexpected test Kubernetes request {method} {path}"),
                    };
                    Ok::<_, std::io::Error>(
                        Response::builder()
                            .status(code)
                            .body(Body::from(serde_json::to_vec(&value).unwrap()))
                            .unwrap(),
                    )
                }
            }
        });
        let client = Client::new(service, "sandbox-ns");
        let ledger =
            K8sWorkerCreateLedger::new(client.clone(), "control-ns", "sandbox-ns", "provider-a")
                .unwrap();
        let (ledger_name, ledger_uid) = ledger
            .initialize("sandbox-ns", "worker", "pod-uid")
            .await
            .unwrap();
        let anchor = WorkerCreateLedger::new(
            "provider-a",
            "control-ns",
            ledger_name,
            ledger_uid,
            ledger.configuration_fingerprint(),
        )
        .unwrap();
        let directory = Arc::new(MemoryWorkerDirectory::new());
        let clock = Arc::new(ManualClock::new(100));
        let registered = directory
            .register(
                WorkerRegistration {
                    worker_id: "k8s:sandbox-ns:worker:pod-uid".into(),
                    incarnation_id: "pod-uid".into(),
                    manifest: WorkerManifest::default(),
                    create_ledger: Some(anchor),
                },
                100,
                60_000,
            )
            .await
            .unwrap();
        let identity = registered.snapshot.identity;
        directory
            .heartbeat(
                &identity,
                WorkerHeartbeat {
                    sequence: 1,
                    ready: true,
                    in_flight: 0,
                    warm_environment_shapes: Default::default(),
                    credential_observations: Default::default(),
                    acp_capability_observations: Default::default(),
                },
                100,
                60_000,
            )
            .await
            .unwrap();
        let broker = K8sCreateBroker::new(client, ledger, "sandbox-ns", "provider-a");
        let create = K8sCreateRequest {
            path: "/api/v1/namespaces/sandbox-ns/pods".into(),
            object: json!({"metadata":{
                "name":"sandbox-pod", "namespace":"sandbox-ns",
                "annotations":{"awaken.dev/realization-digest":"a".repeat(64)}
            }}),
        };
        let first = tokio::spawn({
            let (broker, directory, clock, identity, create) = (
                broker.clone(),
                directory.clone(),
                clock.clone(),
                identity.clone(),
                create.clone(),
            );
            async move { broker.create(directory, clock, identity, create).await }
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), started.notified())
            .await
            .unwrap();
        assert_eq!(pod_posts.load(Ordering::SeqCst), 1, "R1");
        assert!(
            broker
                .create(
                    directory.clone(),
                    clock.clone(),
                    identity.clone(),
                    create.clone()
                )
                .await
                .is_err(),
            "R2 InFlight retry"
        );
        assert_eq!(
            pod_posts.load(Ordering::SeqCst),
            1,
            "R2 no duplicate API request"
        );
        first.abort();
        release.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(5), completed.notified())
            .await
            .unwrap();
        let replay = broker
            .create(directory.clone(), clock, identity.clone(), create)
            .await
            .unwrap();
        assert_eq!(
            replay.status, 409,
            "R4 typed verifier must GET the existing Pod"
        );
        assert_eq!(
            pod_posts.load(Ordering::SeqCst),
            1,
            "R3 independent executor made exactly one API call"
        );
        let predecessor = ContainerEffectFence::new(
            "effect-a",
            &identity.worker_id,
            identity.lease_owner(),
            1,
            u64::MAX,
        )
        .unwrap();
        assert_eq!(
            broker
                .seal_predecessor(directory.clone(), "scope", &predecessor, "awaken-scope")
                .await
                .unwrap(),
            RequestJoin::Unknown,
            "E5 a live Worker is not closed by a completed Create alone"
        );
        directory.deregister(&identity).await.unwrap();
        let process = json!({
            "namespace":"sandbox-ns", "name":"worker", "uid":"pod-uid",
            "node":"node-a", "containers":[], "retained":true
        });
        directory
            .record_process_exit(
                &identity,
                awaken_dispatch::WorkerProcessExit::new(
                    "provider-a",
                    process.to_string(),
                    "resource-version-7",
                )
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            matches!(
                broker.seal_predecessor(directory, "scope", &predecessor, "awaken-scope").await.unwrap(),
                RequestJoin::Complete(tickets) if tickets.len() == 1
            ),
            "E5 exact persisted exit plus one finished ticket joins"
        );
    }

    #[tokio::test]
    async fn private_create_route_requires_transport_and_current_registry_identity() {
        // Causes: A no authenticated header, B authenticated other Worker,
        // C matching header with no durable Registry incarnation. Effects:
        // each is refused before a provider GET, ticket CAS or Kubernetes
        // Create; neither application JSON nor a copied Worker id is proof.
        let api_calls = Arc::new(AtomicUsize::new(0));
        let service = tower::service_fn({
            let api_calls = api_calls.clone();
            move |_request: Request<Body>| {
                api_calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    Ok::<_, std::io::Error>(
                        Response::builder()
                            .status(500)
                            .body(Body::from(b"{}".to_vec()))
                            .unwrap(),
                    )
                }
            }
        });
        let client = Client::new(service, "sandbox-ns");
        let ledger =
            K8sWorkerCreateLedger::new(client.clone(), "control-ns", "sandbox-ns", "provider-a")
                .unwrap();
        let router = Arc::new(K8sCreateBroker::new(
            client,
            ledger,
            "sandbox-ns",
            "provider-a",
        ))
        .router(
            Arc::new(MemoryWorkerDirectory::new()),
            Arc::new(HeaderWorkerAuthenticator),
            Arc::new(ManualClock::new(100)),
        );
        let wire = json!({
            "identity": WorkerIdentity::new("k8s:sandbox-ns:worker:pod-uid", "pod-uid", 1),
            "create": {
                "path":"/api/v1/namespaces/sandbox-ns/pods",
                "object":{"metadata":{"name":"pod","namespace":"sandbox-ns"}}
            }
        });
        let body = serde_json::to_vec(&wire).unwrap();
        let request = |header: Option<&str>| {
            let mut builder = Request::builder()
                .method(Method::POST)
                .uri(PATH)
                .header(header::CONTENT_TYPE, "application/json");
            if let Some(value) = header {
                builder = builder.header(WORKER_ID_HEADER, value);
            }
            builder.body(AxumBody::from(body.clone())).unwrap()
        };
        assert_eq!(
            router
                .clone()
                .oneshot(request(None))
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED,
            "A"
        );
        assert_eq!(
            router
                .clone()
                .oneshot(request(Some("foreign")))
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST,
            "B"
        );
        assert_eq!(
            router
                .oneshot(request(Some("k8s:sandbox-ns:worker:pod-uid")))
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST,
            "C"
        );
        assert_eq!(
            api_calls.load(Ordering::SeqCst),
            0,
            "no rejected identity reached Kubernetes"
        );
    }
}
