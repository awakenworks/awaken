// @formal surface obligation=sandbox.admission_never_weakens_isolation
// @formal reason: This ConfigMap CAS adapter retains the exact Worker UID and
// @formal reason: orders Create admission, seal and response-loss replay.
// @formal reason: Its tests qualify those transitions; Pod exit and Session
// @formal reason: cleanup still require their independent evidence owners.
//! Provider-owned, process-independent admission root for one Worker Pod.
//!
//! Only Coordinator holds a client with Create authority in the control
//! namespace. The Registry freezes the returned UID before Worker Ready; a
//! replacement process must never initialize a missing ledger to claim proof.

use awaken_container_contract::RuntimeError;
use awaken_container_contract::{CreateReplyProfile, K8sLedgerBinding, RequestJoin, TicketStatus};
mod broker_effects;
mod capacity;
mod capture;
pub(crate) mod isolation;
mod state;
pub use isolation::original_guard_release_required;
pub use isolation::release_original_guard;
pub use isolation::verify_reservation as verify_pod_isolation;

pub use state::WorkerCreateLedgerState;

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::ConfigMap;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::PostParams;
use kube::{Api, Client, ResourceExt};

use crate::PodProcessBinding;
use crate::{api_conflict, backend};

const CONTRACT: &str = "awaken.dev/create-ledger-contract";
const PROVIDER: &str = "awaken.dev/create-ledger-provider";
const SANDBOX_NAMESPACE: &str = "awaken.dev/create-ledger-sandbox-namespace";
const WORKER_NAMESPACE: &str = "awaken.dev/create-ledger-worker-namespace";
const WORKER_NAME: &str = "awaken.dev/create-ledger-worker-name";
const WORKER_UID: &str = "awaken.dev/create-ledger-worker-uid";
const WORKER_INCARNATION: &str = "awaken.dev/create-ledger-worker-incarnation";
const CONFIGURATION: &str = "awaken.dev/create-ledger-configuration";
const VERSION: &str = "v1";
const CAS_ATTEMPTS: usize = 8;

/// One Kubernetes physical effect store, scoped by a retained Worker Pod UID.
#[derive(Clone)]
pub struct K8sWorkerCreateLedger {
    client: Client,
    control_namespace: String,
    worker_namespace: String,
    sandbox_namespace: String,
    provider_id: String,
    configuration_fingerprint: String,
}

impl K8sWorkerCreateLedger {
    pub fn new(
        client: Client,
        control_namespace: impl Into<String>,
        worker_namespace: impl Into<String>,
        sandbox_namespace: impl Into<String>,
        provider_id: impl Into<String>,
    ) -> Result<Self, RuntimeError> {
        let control_namespace = control_namespace.into();
        let worker_namespace = worker_namespace.into();
        let sandbox_namespace = sandbox_namespace.into();
        let provider_id = provider_id.into();
        if [
            control_namespace.as_str(),
            worker_namespace.as_str(),
            sandbox_namespace.as_str(),
            provider_id.as_str(),
        ]
        .into_iter()
        .any(|part| part.trim().is_empty() || part.trim() != part)
            || control_namespace == sandbox_namespace
            || control_namespace == worker_namespace
        {
            return Err(backend(
                "Kubernetes Create ledger requires a separate exact control namespace",
            ));
        }
        let encoded = serde_json::to_vec(&(
            VERSION,
            &control_namespace,
            &sandbox_namespace,
            &provider_id,
        ))
        .map_err(backend)?;
        let configuration_fingerprint = blake3::hash(&encoded).to_hex().to_string();
        Ok(Self {
            client,
            control_namespace,
            worker_namespace,
            sandbox_namespace,
            provider_id,
            configuration_fingerprint,
        })
    }

    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }

    /// One ledger per Worker incarnation. A historical incarnation equal to the
    /// Pod UID keeps its original Pod-scoped name; a container-instance
    /// incarnation of the same Pod gets its own ledger.
    fn ledger_name(
        worker_namespace: &str,
        worker_name: &str,
        worker_uid: &str,
        worker_incarnation: &str,
    ) -> String {
        let identity = if worker_incarnation == worker_uid {
            serde_json::to_vec(&(worker_namespace, worker_name, worker_uid))
        } else {
            serde_json::to_vec(&(
                worker_namespace,
                worker_name,
                worker_uid,
                worker_incarnation,
            ))
        }
        .expect("Worker Pod coordinate is serializable");
        let digest = blake3::hash(&identity).to_hex();
        format!("awaken-create-{}", &digest.as_str()[..32])
    }

    fn desired(
        &self,
        worker_namespace: &str,
        worker_name: &str,
        worker_uid: &str,
        worker_incarnation: &str,
    ) -> ConfigMap {
        let name = Self::ledger_name(
            worker_namespace,
            worker_name,
            worker_uid,
            worker_incarnation,
        );
        let mut annotations = BTreeMap::from([
            (CONTRACT.into(), VERSION.into()),
            (PROVIDER.into(), self.provider_id.clone()),
            (SANDBOX_NAMESPACE.into(), self.sandbox_namespace.clone()),
            (WORKER_NAMESPACE.into(), worker_namespace.into()),
            (WORKER_NAME.into(), worker_name.into()),
            (WORKER_UID.into(), worker_uid.into()),
            (CONFIGURATION.into(), self.configuration_fingerprint.clone()),
        ]);
        if worker_incarnation != worker_uid {
            annotations.insert(WORKER_INCARNATION.into(), worker_incarnation.into());
        }
        ConfigMap {
            metadata: ObjectMeta {
                name: Some(name),
                namespace: Some(self.control_namespace.clone()),
                annotations: Some(annotations),
                ..Default::default()
            },
            data: Some(BTreeMap::from([(
                "ledger".into(),
                WorkerCreateLedgerState::fresh()
                    .encode()
                    .expect("initial Create ledger state is valid"),
            )])),
            ..Default::default()
        }
    }

    fn verify(&self, desired: &ConfigMap, observed: &ConfigMap) -> Result<String, RuntimeError> {
        if observed.name_any() != desired.name_any()
            || observed.metadata.namespace != desired.metadata.namespace
            || observed.metadata.deletion_timestamp.is_some()
            || observed.immutable == Some(true)
            || !desired
                .metadata
                .annotations
                .as_ref()
                .zip(observed.metadata.annotations.as_ref())
                .is_some_and(|(expected, actual)| {
                    expected
                        .iter()
                        .all(|(key, value)| actual.get(key) == Some(value))
                })
        {
            return Err(backend(
                "Kubernetes Create ledger has a foreign or terminating identity",
            ));
        }
        let uid = observed
            .metadata
            .uid
            .as_deref()
            .filter(|uid| !uid.trim().is_empty() && uid.trim() == *uid)
            .ok_or_else(|| backend("Kubernetes Create ledger has no exact UID"))?;
        Ok(uid.to_owned())
    }

    fn verified_worker_object(
        &self,
        worker_namespace: &str,
        worker_name: &str,
        worker_uid: &str,
        worker_incarnation: &str,
    ) -> Result<ConfigMap, RuntimeError> {
        if [
            worker_namespace,
            worker_name,
            worker_uid,
            worker_incarnation,
        ]
        .into_iter()
        .any(|v| v.trim().is_empty() || v.trim() != v)
            || worker_namespace != self.worker_namespace
        {
            return Err(backend(
                "Kubernetes Create ledger Worker Pod identity is invalid",
            ));
        }
        Ok(self.desired(
            worker_namespace,
            worker_name,
            worker_uid,
            worker_incarnation,
        ))
    }

    /// Initialize only during verified Worker registration. A 409 reads the
    /// exact object rather than treating a stable name as proof. The Registry
    /// later rejects any same-incarnation retry that observes another UID.
    pub async fn initialize(
        &self,
        worker_namespace: &str,
        worker_name: &str,
        worker_uid: &str,
        worker_incarnation: &str,
    ) -> Result<(String, String), RuntimeError> {
        let desired = self.verified_worker_object(
            worker_namespace,
            worker_name,
            worker_uid,
            worker_incarnation,
        )?;
        capacity::validate_candidate(&desired, &WorkerCreateLedgerState::fresh())?;
        let name = desired.name_any();
        let api: Api<ConfigMap> = Api::namespaced(self.client.clone(), &self.control_namespace);
        let observed = match api.create(&PostParams::default(), &desired).await {
            Ok(created) => created,
            Err(error) if api_conflict(&error) => api.get(&name).await.map_err(backend)?,
            Err(error) => return Err(backend(error)),
        };
        let uid = self.verify(&desired, &observed)?;
        Ok((name, uid))
    }

    /// The Worker incarnation that the ledger named by an anchor belongs to: its
    /// incarnation annotation, or the Pod UID for a historical Pod-scoped
    /// ledger. Identity only; `existing_worker_anchor` still verifies it.
    pub async fn worker_incarnation_of(
        &self,
        ledger_name: &str,
        worker_uid: &str,
    ) -> Result<String, RuntimeError> {
        let api: Api<ConfigMap> = Api::namespaced(self.client.clone(), &self.control_namespace);
        let observed = api.get(ledger_name).await.map_err(backend)?;
        Ok(observed
            .metadata
            .annotations
            .as_ref()
            .and_then(|annotations| annotations.get(WORKER_INCARNATION))
            .cloned()
            .unwrap_or_else(|| worker_uid.to_owned()))
    }

    /// Read an existing original bootstrap anchor. Recovery cannot create a
    /// replacement ledger to turn missing historical evidence into emptiness.
    pub async fn existing_worker_anchor(
        &self,
        worker_namespace: &str,
        worker_name: &str,
        worker_uid: &str,
        worker_incarnation: &str,
    ) -> Result<(String, String), RuntimeError> {
        let desired = self.verified_worker_object(
            worker_namespace,
            worker_name,
            worker_uid,
            worker_incarnation,
        )?;
        let api: Api<ConfigMap> = Api::namespaced(self.client.clone(), &self.control_namespace);
        let observed = api.get(&desired.name_any()).await.map_err(backend)?;
        Ok((desired.name_any(), self.verify(&desired, &observed)?))
    }

    fn expected(&self, binding: K8sLedgerBinding<'_>) -> Result<ConfigMap, RuntimeError> {
        if binding.worker_namespace != self.worker_namespace
            || binding.configuration_fingerprint != self.configuration_fingerprint
            || binding.ledger_uid.trim().is_empty()
            || binding.ledger_uid.trim() != binding.ledger_uid
        {
            return Err(backend(
                "Kubernetes Create ledger differs from the frozen Worker binding",
            ));
        }
        let desired = self.desired(
            binding.worker_namespace,
            binding.worker_name,
            binding.worker_uid,
            binding.worker_incarnation,
        );
        if desired.name_any() != binding.ledger_name {
            return Err(backend(
                "Kubernetes Create ledger has another Worker Pod coordinate",
            ));
        }
        Ok(desired)
    }

    async fn load_exact(
        &self,
        binding: K8sLedgerBinding<'_>,
    ) -> Result<(ConfigMap, WorkerCreateLedgerState), RuntimeError> {
        let desired = self.expected(binding)?;
        let api: Api<ConfigMap> = Api::namespaced(self.client.clone(), &self.control_namespace);
        let observed = api.get(binding.ledger_name).await.map_err(backend)?;
        if self.verify(&desired, &observed)? != binding.ledger_uid
            || observed
                .metadata
                .resource_version
                .as_deref()
                .is_none_or(|version| version.trim().is_empty())
        {
            return Err(backend(
                "Kubernetes Create ledger UID or resourceVersion changed",
            ));
        }
        let encoded = observed
            .data
            .as_ref()
            .and_then(|data| data.get("ledger"))
            .ok_or_else(|| backend("Kubernetes Create ledger state is missing"))?;
        capacity::validate_observed(&observed)?;
        let state = WorkerCreateLedgerState::decode(encoded)?;
        Ok((observed, state))
    }

    /// Read, decide and replace one exact ConfigMap under Kubernetes
    /// resourceVersion CAS. Exhausted conflicts are Unknown, never proof of
    /// absence or permission to resend an already admitted Create.
    async fn cas_object<R>(
        &self,
        binding: K8sLedgerBinding<'_>,
        transition: impl Fn(
            &mut ConfigMap,
            &mut WorkerCreateLedgerState,
        ) -> Result<(R, bool), RuntimeError>,
    ) -> Result<R, RuntimeError> {
        let api: Api<ConfigMap> = Api::namespaced(self.client.clone(), &self.control_namespace);
        let desired = self.expected(binding)?;
        for _ in 0..CAS_ATTEMPTS {
            let (mut object, mut state) = self.load_exact(binding).await?;
            let (result, changed) = transition(&mut object, &mut state)?;
            if changed || state.reply_profile() == CreateReplyProfile::FreshCreate {
                object
                    .data
                    .get_or_insert_with(Default::default)
                    .insert("ledger".into(), state.encode()?);
                capacity::validate_candidate(&object, &state)?;
            }
            if !changed {
                return Ok(result);
            }
            match api
                .replace(binding.ledger_name, &PostParams::default(), &object)
                .await
            {
                Ok(updated) => {
                    if self.verify(&desired, &updated)? != binding.ledger_uid {
                        return Err(backend("Kubernetes Create ledger replacement changed UID"));
                    }
                    if updated.data.as_ref().and_then(|data| data.get("ledger"))
                        != object.data.as_ref().and_then(|data| data.get("ledger"))
                    {
                        return Err(backend(
                            "Kubernetes Create ledger replacement changed admitted bytes",
                        ));
                    }
                    // A mutating admission response can already consume the
                    // reserve. Do not grant Forward merely because UID/RV CAS
                    // succeeded; the original ticket remains pending instead.
                    capacity::validate_candidate(&updated, &state)?;
                    return Ok(result);
                }
                Err(error) if api_conflict(&error) => continue,
                Err(error) => return Err(backend(error)),
            }
        }
        Err(backend("Kubernetes Create ledger CAS did not converge"))
    }

    async fn mutate<R>(
        &self,
        binding: K8sLedgerBinding<'_>,
        transition: impl Fn(&mut WorkerCreateLedgerState) -> Result<(R, bool), RuntimeError>,
    ) -> Result<R, RuntimeError> {
        self.cas_object(binding, |_, state| transition(state)).await
    }

    /// Retain the original running Pod/container coordinates in the same
    /// externally anchored ledger before Worker Ready. A later registration
    /// retry may replay identical bytes but cannot attach a replacement body.
    pub async fn retain_process_binding(
        &self,
        binding: K8sLedgerBinding<'_>,
        process: &PodProcessBinding,
    ) -> Result<(), RuntimeError> {
        if !process.matches_pod(
            binding.worker_namespace,
            binding.worker_name,
            binding.worker_uid,
        ) || !process.worker_evidence_was_retained()
            || !process.belongs_to_incarnation(binding.worker_uid, binding.worker_incarnation)
        {
            return Err(backend(
                "Worker Create ledger received an unretained or foreign Pod binding",
            ));
        }
        let encoded = serde_json::to_string(process).map_err(backend)?;
        self.cas_object(binding, |object, state| {
            let data = object.data.get_or_insert_with(Default::default);
            match data.get("worker_process_binding") {
                Some(existing) if existing == &encoded => Ok(((), false)),
                Some(_) => Err(backend(
                    "Worker Create ledger cannot replace its process binding",
                )),
                None => {
                    if !state.admits_first_process_binding() {
                        return Err(backend(
                            "sealed Worker cannot admit its first process binding",
                        ));
                    }
                    data.insert("worker_process_binding".into(), encoded.clone());
                    Ok(((), true))
                }
            }
        })
        .await
    }

    /// A missing or tampered process binding remains Unknown. The enclosing
    /// Registry UID and provider configuration were rechecked by `load_exact`.
    pub async fn read_process_binding(
        &self,
        binding: K8sLedgerBinding<'_>,
    ) -> Result<Option<PodProcessBinding>, RuntimeError> {
        let (object, _) = self.load_exact(binding).await?;
        let Some(encoded) = object
            .data
            .as_ref()
            .and_then(|data| data.get("worker_process_binding"))
        else {
            return Ok(None);
        };
        let process: PodProcessBinding = serde_json::from_str(encoded).map_err(backend)?;
        if !process.matches_pod(
            binding.worker_namespace,
            binding.worker_name,
            binding.worker_uid,
        ) || !process.worker_evidence_was_retained()
            || !process.belongs_to_incarnation(binding.worker_uid, binding.worker_incarnation)
        {
            return Err(backend(
                "Worker Create ledger process binding no longer matches its Pod",
            ));
        }
        Ok(Some(process))
    }

    /// Read only the original ledger's already sealed Worker group. Missing,
    /// foreign or corrupt physical evidence is never initialized or repaired.
    /// The result joins admitted Creates only: it grants no execution stop,
    /// participant absence, target access or Session cleanup authority.
    pub async fn read_sealed_worker_group(
        &self,
        binding: K8sLedgerBinding<'_>,
        group: &str,
    ) -> Result<RequestJoin, RuntimeError> {
        let (_, state) = self.load_exact(binding).await?;
        Ok(state.join_sealed_worker_group(group))
    }

    /// The existing sealed ledger is the only request authority. After the
    /// trusted caller closes the original Worker's Registry admission, race an inert
    /// same-Kind object against each unresolved original POST and retain its
    /// observed UID in that same ticket. Participant disposal remains separate.
    pub async fn isolate_sealed_group_and_join(
        &self,
        binding: K8sLedgerBinding<'_>,
        group: &str,
    ) -> Result<RequestJoin, RuntimeError> {
        let (_, state) = self.load_exact(binding).await?;
        let isolation_client = capture::isolation_client(
            &self.client,
            state.reply_profile() == CreateReplyProfile::FreshCreate,
        );
        let Some(tickets) = state.sealed_worker_group_tickets(group) else {
            return Ok(RequestJoin::Unknown);
        };
        for ticket in tickets {
            if !matches!(ticket.status, TicketStatus::InFlight) && !ticket.isolation_pending {
                continue;
            }
            // A historical ticket admitted before provider nonces existed
            // cannot borrow a newly minted reservation after process loss.
            if ticket.isolation_id.is_empty() {
                return Ok(RequestJoin::Unknown);
            }
            let should_isolate = self
                .mutate(binding, |state| state.start_isolation(&ticket.request))
                .await?;
            if !should_isolate {
                continue;
            }
            let Some(result) = isolation::isolate(&isolation_client, &ticket).await? else {
                return Ok(RequestJoin::Unknown);
            };
            self.mutate(binding, |state| {
                let changed = state.finish_isolation(&ticket.request, result.clone())?;
                Ok(((), changed))
            })
            .await?;
        }
        self.read_sealed_worker_group(binding, group).await
    }

    /// Deny future first process binding and Create entry in one original
    /// ConfigMap CAS. This never initializes a missing ledger and does not
    /// assert Registry absence, physical exit or Session cleanup completion.
    pub async fn seal_unattempted_worker(
        &self,
        binding: K8sLedgerBinding<'_>,
    ) -> Result<(), RuntimeError> {
        self.cas_object(binding, |object, state| {
            if object
                .data
                .as_ref()
                .is_some_and(|data| data.contains_key("worker_process_binding"))
            {
                return Err(backend(
                    "Worker bootstrap already retained a process participant",
                ));
            }
            Ok(((), state.seal_unattempted_worker()?))
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    mod finite_policy;
    use super::*;
    use awaken_container_contract::{
        CreateTicketRequest, K8sCreateBrokerEffects, TicketAdmission, TicketResult,
    };
    use http::{Request, Response};
    use kube::client::Body;
    use serde_json::{Value, json};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    struct Fixture {
        ledger: K8sWorkerCreateLedger,
        stored: Arc<Mutex<Option<Value>>>,
        calls: Arc<Mutex<Vec<String>>>,
        conflict: Arc<AtomicBool>,
        conflicting_data: Arc<Mutex<Option<Value>>>,
        lose_finish_reply: Arc<AtomicBool>,
    }

    fn fixture(lose_first_response: bool) -> Fixture {
        let stored = Arc::new(Mutex::new(None::<Value>));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let lost = Arc::new(Mutex::new(false));
        let conflict = Arc::new(AtomicBool::new(false));
        let conflicting_data = Arc::new(Mutex::new(None::<Value>));
        let lose_finish_reply = Arc::new(AtomicBool::new(false));
        let service = tower::service_fn({
            let (stored, calls, lost, conflict, lose_finish_reply, conflicting_data) = (
                stored.clone(),
                calls.clone(),
                lost.clone(),
                conflict.clone(),
                lose_finish_reply.clone(),
                conflicting_data.clone(),
            );
            move |request: Request<Body>| {
                let (stored, calls, lost, conflict, lose_finish_reply, conflicting_data) = (
                    stored.clone(),
                    calls.clone(),
                    lost.clone(),
                    conflict.clone(),
                    lose_finish_reply.clone(),
                    conflicting_data.clone(),
                );
                async move {
                    let path = request.uri().path().to_owned();
                    assert!(path.starts_with("/api/v1/namespaces/awaken-control/configmaps"));
                    let method = request.method().as_str().to_owned();
                    calls.lock().unwrap().push(method.clone());
                    let bytes = request.into_body().collect_bytes().await.unwrap();
                    let request_body: Value = if bytes.is_empty() {
                        Value::Null
                    } else {
                        serde_json::from_slice(&bytes).unwrap()
                    };
                    let (status, body) = match method.as_str() {
                        "POST" => {
                            let mut guard = stored.lock().unwrap();
                            if guard.is_none() {
                                let mut value = request_body;
                                value["apiVersion"] = json!("v1");
                                value["kind"] = json!("ConfigMap");
                                value["metadata"]["uid"] = json!("uid-original");
                                value["metadata"]["resourceVersion"] = json!("1");
                                *guard = Some(value.clone());
                                if lose_first_response && !*lost.lock().unwrap() {
                                    *lost.lock().unwrap() = true;
                                    return Err(std::io::Error::new(
                                        std::io::ErrorKind::ConnectionReset,
                                        "Create response lost after admission",
                                    ));
                                }
                                (201, value)
                            } else {
                                (409, json!({"apiVersion":"v1","kind":"Status","status":"Failure","code":409,"reason":"AlreadyExists"}))
                            }
                        }
                        "GET" => stored.lock().unwrap().clone().map_or_else(
                            || (404, json!({"apiVersion":"v1","kind":"Status","status":"Failure","code":404,"reason":"NotFound"})),
                            |value| {
                                let status = if value["kind"].as_str() == Some("Status") {
                                    u16::try_from(value["code"].as_u64().unwrap()).unwrap()
                                } else {
                                    200
                                };
                                (status, value)
                            },
                        ),
                        "PUT" => {
                            let mut current = stored.lock().unwrap();
                            let existing = current.as_mut().expect("PUT follows GET");
                            if conflict.swap(false, Ordering::SeqCst) {
                                if let Some(data)=conflicting_data.lock().unwrap().take() {existing["data"]=data;}
                                existing["metadata"]["resourceVersion"] = json!("2");
                                (409, json!({"apiVersion":"v1","kind":"Status","status":"Failure","code":409,"reason":"Conflict"}))
                            } else if request_body["metadata"]["uid"] != existing["metadata"]["uid"]
                                || request_body["metadata"]["resourceVersion"] != existing["metadata"]["resourceVersion"]
                            {
                                (409, json!({"apiVersion":"v1","kind":"Status","status":"Failure","code":409,"reason":"Conflict"}))
                            } else {
                                let mut value = request_body;
                                let next = existing["metadata"]["resourceVersion"]
                                    .as_str()
                                    .unwrap()
                                    .parse::<u64>()
                                    .unwrap() + 1;
                                value["metadata"]["resourceVersion"] = json!(next.to_string());
                                *existing = value.clone();
                                if lose_finish_reply.swap(false, Ordering::SeqCst) {
                                    return Err(std::io::Error::new(std::io::ErrorKind::ConnectionReset, "finish reply lost after exact CAS"));
                                }
                                (200, value)
                            }
                        }
                        _ => panic!("unexpected ledger method {method}"),
                    };
                    Ok::<_, std::io::Error>(
                        Response::builder()
                            .status(status)
                            .body(Body::from(serde_json::to_vec(&body).unwrap()))
                            .unwrap(),
                    )
                }
            }
        });
        Fixture {
            ledger: K8sWorkerCreateLedger::new(
                Client::new(service, "awaken-control"),
                "awaken-control",
                "awaken-system",
                "awaken-system",
                "provider-a",
            )
            .unwrap(),
            stored,
            calls,
            conflict,
            conflicting_data,
            lose_finish_reply,
        }
    }

    #[tokio::test]
    async fn registration_ledger_join_retains_exact_api_uid() {
        // Causes: C1 first Create succeeds or response is lost after admission;
        // C2 replay observes exact, replaced-UID, foreign metadata, immutable
        // or missing-UID object. Effects: E1 exact UID can be frozen, E2 lost response
        // gives no witness until 409+GET joins it, E3 replacement returns a
        // different UID for Registry rejection, E4 malformed/foreign object
        // never becomes an anchor. Rules J1=C1 success=>E1, J2=lost+exact=>E2,
        // J3=replaced=>E3, J4=foreign/missing=>E4.
        let Fixture {
            ledger,
            stored,
            calls,
            ..
        } = fixture(true);
        assert!(
            ledger
                .initialize("awaken-system", "worker", "pod-uid", "pod-uid")
                .await
                .is_err(),
            "J2 no response-only proof"
        );
        let (name, uid) = ledger
            .initialize("awaken-system", "worker", "pod-uid", "pod-uid")
            .await
            .unwrap();
        assert!(name.starts_with("awaken-create-"));
        assert_eq!(uid, "uid-original", "J2 joined API UID");
        assert_eq!(*calls.lock().unwrap(), ["POST", "POST", "GET"]);

        stored.lock().unwrap().as_mut().unwrap()["metadata"]["uid"] = json!("uid-replaced");
        assert_eq!(
            ledger
                .initialize("awaken-system", "worker", "pod-uid", "pod-uid")
                .await
                .unwrap()
                .1,
            "uid-replaced",
            "J3 provider cannot mistake the stable name for the old UID"
        );
        stored.lock().unwrap().as_mut().unwrap()["metadata"]["annotations"][PROVIDER] =
            json!("foreign");
        assert!(
            ledger
                .initialize("awaken-system", "worker", "pod-uid", "pod-uid")
                .await
                .is_err(),
            "J4 foreign provider"
        );
        stored.lock().unwrap().as_mut().unwrap()["metadata"]["annotations"][PROVIDER] =
            json!("provider-a");
        stored.lock().unwrap().as_mut().unwrap()["immutable"] = json!(true);
        assert!(
            ledger
                .initialize("awaken-system", "worker", "pod-uid", "pod-uid")
                .await
                .is_err(),
            "J4 immutable ledger cannot accept ticket CAS"
        );
        stored.lock().unwrap().as_mut().unwrap()["immutable"] = json!(false);
        stored.lock().unwrap().as_mut().unwrap()["metadata"]["uid"] = json!("");
        assert!(
            ledger
                .initialize("awaken-system", "worker", "pod-uid", "pod-uid")
                .await
                .is_err(),
            "J4 missing UID"
        );
    }

    #[tokio::test]
    async fn registration_ledger_rejects_unseparated_or_foreign_worker_scope() {
        // C1 control namespace equals Sandbox namespace -> constructor fails
        // before I/O; C2 Worker Pod names another configured Worker namespace
        // -> no Create request, independently from the Sandbox target. Both effects keep the control ledger off the Worker
        // ServiceAccount's Sandbox mutation domain.
        let Fixture { ledger, calls, .. } = fixture(false);
        assert!(
            ledger
                .initialize("foreign", "worker", "pod-uid", "pod-uid")
                .await
                .is_err()
        );
        assert!(calls.lock().unwrap().is_empty());
        assert!(
            K8sWorkerCreateLedger::new(
                ledger.client.clone(),
                "awaken-system",
                "awaken-system",
                "awaken-system",
                "provider-a"
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn worker_identity_and_sandbox_targets_keep_separate_exact_namespaces() {
        // N1 configured Worker, Sandbox and control namespace roles are exact
        // and control differs from both -> original anchor initialization and
        // frozen-binding lookup succeed, retaining both namespace annotations.
        // N2 a Sandbox/foreign namespace offered as Worker identity -> reject
        // before API I/O. N3 control overlaps either role -> constructor rejects.
        // The existing CAS fixture owns persistence; no alternate ledger exists.
        let Fixture {
            ledger,
            stored,
            calls,
            ..
        } = fixture(false);
        let isolated = K8sWorkerCreateLedger::new(
            ledger.client.clone(),
            "awaken-control",
            "worker-control",
            "tenant-sandboxes",
            "provider-a",
        )
        .unwrap();
        let (name, uid) = isolated
            .initialize("worker-control", "worker", "pod-uid", "pod-uid")
            .await
            .unwrap();
        let observed = stored.lock().unwrap().clone().unwrap();
        assert_eq!(
            observed["metadata"]["annotations"][WORKER_NAMESPACE], "worker-control",
            "N1 Worker"
        );
        assert_eq!(
            observed["metadata"]["annotations"][SANDBOX_NAMESPACE], "tenant-sandboxes",
            "N1 Sandbox"
        );
        let binding = K8sLedgerBinding {
            worker_namespace: "worker-control",
            worker_name: "worker",
            worker_uid: "pod-uid",
            worker_incarnation: "pod-uid",
            ledger_name: &name,
            ledger_uid: &uid,
            configuration_fingerprint: isolated.configuration_fingerprint(),
        };
        assert!(isolated.expected(binding).is_ok(), "N1 original binding");
        let count = calls.lock().unwrap().len();
        assert!(
            isolated
                .initialize("tenant-sandboxes", "worker", "pod-uid", "pod-uid")
                .await
                .is_err(),
            "N2 no confusion"
        );
        assert!(
            isolated
                .expected(K8sLedgerBinding {
                    worker_namespace: "tenant-sandboxes",
                    ..binding
                })
                .is_err(),
            "N2 frozen identity"
        );
        assert_eq!(calls.lock().unwrap().len(), count, "N2 no API I/O");
        for control in ["worker-control", "tenant-sandboxes"] {
            assert!(
                K8sWorkerCreateLedger::new(
                    ledger.client.clone(),
                    control,
                    "worker-control",
                    "tenant-sandboxes",
                    "provider-a"
                )
                .is_err(),
                "N3 {control}"
            );
        }
    }

    #[tokio::test]
    async fn ticket_admission_and_close_share_the_real_configmap_cas() {
        // Causes: C1 exact Registry-frozen UID; C2 an unrelated writer wins
        // the first resourceVersion CAS; C3 this request is new/replayed;
        // C4 close wins after admission; C5 independent executor records the
        // API UID; C6 same-name ledger is replaced. Effects: E1 only the
        // retried successful CAS grants Forward, E2 replay never forwards,
        // E3 close waits on InFlight, E4 durable completion joins, E5 a foreign
        // ledger UID rejects before mutation. Rules T1=C1+C2+C3=>E1,
        // T2=T1+replay=>E2, T3=T1+C4+!C5=>E3, T4=T3+C5=>E4,
        // T5=C6=>E5. No mock cleanup receipt is returned.
        // Pure-read decision table: Q1 exact UID/header, both seals, finished
        // requests -> exact complete set (including explicitly sealed empty);
        // Q2 missing Worker seal / Q3 open or absent group -> Unknown;
        // Q4 InFlight or isolationPending -> Unknown, without completing it;
        // Q5 foreign UID/header/config, corrupt codec, 404 or 403 -> error,
        // never an empty replacement ledger/group. Q6 a historical missing
        // nonce remains missing in returned data, not a physical closure grant.
        // Q7 a fresh adapter and repeat read return the same value/bytes.
        // Every Q row performs exactly two GETs and no other HTTP method,
        // preserving the server's entire original object and resourceVersion.
        // The existing service supplies transport faults as Kubernetes Status
        // replies; it is not a second ledger or a fabricated cleanup receipt.
        let Fixture {
            ledger,
            stored,
            calls,
            conflict,
            ..
        } = fixture(false);
        let fingerprint = ledger.configuration_fingerprint().to_owned();
        let (name, uid) = ledger
            .initialize("awaken-system", "worker", "pod-uid", "pod-uid")
            .await
            .unwrap();
        let binding = K8sLedgerBinding {
            worker_namespace: "awaken-system",
            worker_name: "worker",
            worker_uid: "pod-uid",
            worker_incarnation: "pod-uid",
            ledger_name: &name,
            ledger_uid: &uid,
            configuration_fingerprint: &fingerprint,
        };
        let request = CreateTicketRequest::new(
            "effect-a",
            "pods",
            "awaken-system",
            "sandbox-pod",
            "a".repeat(64),
        )
        .unwrap();
        conflict.store(true, Ordering::SeqCst);
        assert_eq!(
            ledger.admit_request(binding, &request).await.unwrap().0,
            TicketAdmission::Forward,
            "T1"
        );
        let writes = calls
            .lock()
            .unwrap()
            .iter()
            .filter(|method| *method == "PUT")
            .count();
        assert_eq!(writes, 2, "T1 one conflicted CAS and one winner");
        assert_eq!(
            ledger.admit_request(binding, &request).await.unwrap().0,
            TicketAdmission::InFlight,
            "T2"
        );
        assert_eq!(
            calls
                .lock()
                .unwrap()
                .iter()
                .filter(|method| *method == "PUT")
                .count(),
            writes,
            "T2 retry does not mutate or forward"
        );
        assert_eq!(
            ledger
                .seal_group_and_join(binding, "effect-a")
                .await
                .unwrap(),
            RequestJoin::Unknown,
            "T3"
        );
        ledger
            .finish_request(
                binding,
                &request,
                &TicketResult::Created {
                    uid: "sandbox-uid".into(),
                },
            )
            .await
            .unwrap();
        assert!(
            matches!(ledger.seal_group_and_join(binding, "effect-a").await.unwrap(), RequestJoin::Complete(tickets) if tickets.len() == 1),
            "T4"
        );
        let ticket = ledger.ticket_for(binding, &request).await.unwrap().unwrap();
        ledger.seal_worker(binding).await.unwrap();
        let original = stored.lock().unwrap().clone().unwrap();

        stored.lock().unwrap().as_mut().unwrap()["metadata"]["uid"] = json!("replacement-uid");
        let writes = calls
            .lock()
            .unwrap()
            .iter()
            .filter(|method| *method == "PUT")
            .count();
        assert!(
            ledger
                .seal_group_and_join(binding, "effect-a")
                .await
                .is_err(),
            "T5"
        );
        assert_eq!(
            calls
                .lock()
                .unwrap()
                .iter()
                .filter(|method| *method == "PUT")
                .count(),
            writes,
            "T5 no foreign mutation"
        );

        let client = ledger.client.clone();
        drop(ledger);
        let cold = K8sWorkerCreateLedger::new(
            client,
            "awaken-control",
            "awaken-system",
            "awaken-system",
            "provider-a",
        )
        .unwrap();
        for cause in [
            "complete",
            "empty",
            "worker-open",
            "group-open",
            "group-missing",
            "in-flight",
            "isolation-pending",
            "foreign-uid",
            "foreign-header",
            "foreign-config",
            "bad-codec",
            "404",
            "403",
            "legacy-nonce",
        ] {
            let mut object = original.clone();
            let mut state: Value =
                serde_json::from_str(object["data"]["ledger"].as_str().unwrap()).unwrap();
            let mut group = "effect-a";
            let mut expected = Some(RequestJoin::Complete(vec![ticket.clone()]));
            match cause {
                "worker-open" => {
                    state["worker_sealed"] = json!(false);
                    expected = Some(RequestJoin::Unknown);
                }
                "group-open" => {
                    state["groups"][group] = json!("open");
                    expected = Some(RequestJoin::Unknown);
                }
                "group-missing" => {
                    group = "never-observed";
                    expected = Some(RequestJoin::Unknown);
                }
                "in-flight" => {
                    state["tickets"][request.key()]["status"] = json!("in_flight");
                    expected = Some(RequestJoin::Unknown);
                }
                "isolation-pending" => {
                    state["tickets"][request.key()]["isolation_pending"] = json!(true);
                    expected = Some(RequestJoin::Unknown);
                }
                "empty" => {
                    state["tickets"] = json!({});
                    expected = Some(RequestJoin::Complete(Vec::new()));
                }
                "foreign-uid" => {
                    object["metadata"]["uid"] = json!("replacement-uid");
                    expected = None;
                }
                "foreign-header" => {
                    object["metadata"]["annotations"][WORKER_UID] = json!("foreign-pod");
                    expected = None;
                }
                "foreign-config" => {
                    object["metadata"]["annotations"][CONFIGURATION] = json!("foreign-config");
                    expected = None;
                }
                "bad-codec" | "404" | "403" => expected = None,
                "legacy-nonce" => {
                    state["tickets"][request.key()]
                        .as_object_mut()
                        .unwrap()
                        .remove("isolation_id");
                    let mut historical = ticket.clone();
                    historical.isolation_id.clear();
                    expected = Some(RequestJoin::Complete(vec![historical]));
                }
                "complete" => {}
                _ => unreachable!(),
            }
            object["data"]["ledger"] = json!(state.to_string());
            if cause == "bad-codec" {
                object["data"]["ledger"] = json!("not-json");
            } else if cause == "403" {
                object = json!({"apiVersion":"v1","kind":"Status","status":"Failure","code":403,"reason":"Forbidden"});
            }
            let before = (cause != "404").then_some(object);
            *stored.lock().unwrap() = before.clone();
            let first_call = calls.lock().unwrap().len();
            for _ in 0..2 {
                let observed = cold.read_sealed_worker_group(binding, group).await;
                match &expected {
                    Some(expected) => assert_eq!(observed.unwrap(), *expected, "Q1–Q7/{cause}"),
                    None => assert!(observed.is_err(), "Q5/{cause}"),
                }
            }
            assert_eq!(
                &calls.lock().unwrap()[first_call..],
                ["GET", "GET"],
                "Q1–Q7/{cause} only GET"
            );
            assert_eq!(
                *stored.lock().unwrap(),
                before,
                "Q1–Q7/{cause} exact bytes remain"
            );
        }
    }

    #[tokio::test]
    async fn bootstrap_anchor_reads_original_uid_without_initializing_missing_evidence() {
        // A1 original exact object -> return original name/UID using GET only.
        // A2 missing/foreign/terminating object -> error, no POST/PUT/backfill.
        for cause in ["exact", "missing", "foreign", "terminating"] {
            let Fixture {
                ledger,
                stored,
                calls,
                ..
            } = fixture(false);
            let expected = ledger
                .initialize("awaken-system", "worker", "pod-uid", "pod-uid")
                .await
                .unwrap();
            {
                let mut guard = stored.lock().unwrap();
                match cause {
                    "missing" => *guard = None,
                    "foreign" => {
                        guard.as_mut().unwrap()["metadata"]["annotations"][WORKER_UID] =
                            json!("foreign")
                    }
                    "terminating" => {
                        guard.as_mut().unwrap()["metadata"]["deletionTimestamp"] =
                            json!("2026-10-15T00:00:00Z")
                    }
                    _ => {}
                }
            }
            let before = stored.lock().unwrap().clone();
            let start = calls.lock().unwrap().len();
            let observed = ledger
                .existing_worker_anchor("awaken-system", "worker", "pod-uid", "pod-uid")
                .await;
            assert_eq!(observed.is_ok(), cause == "exact", "A1/A2 {cause}");
            if cause == "exact" {
                assert_eq!(observed.unwrap(), expected);
            }
            assert_eq!(&calls.lock().unwrap()[start..], ["GET"], "A1/A2 read-only");
            assert_eq!(
                *stored.lock().unwrap(),
                before,
                "A2 retained original bytes"
            );
        }
    }

    #[tokio::test]
    async fn bootstrap_seal_and_binding_recompute_after_competing_cas() {
        // X1 seal PUT races an already-admitted process -> reload sees binding,
        // refuse empty-bootstrap closure. X2 first-binding PUT races seal ->
        // reload sees closed admission and refuses. Neither stale writer may
        // overwrite the competing owner's original bytes.
        for seal_wins in [false, true] {
            let Fixture {
                ledger,
                stored,
                conflict,
                conflicting_data,
                ..
            } = fixture(false);
            let (name, uid) = ledger
                .initialize("awaken-system", "worker", "pod-uid", "pod-uid")
                .await
                .unwrap();
            let binding = K8sLedgerBinding {
                worker_namespace: "awaken-system",
                worker_name: "worker",
                worker_uid: "pod-uid",
                worker_incarnation: "pod-uid",
                ledger_name: &name,
                ledger_uid: &uid,
                configuration_fingerprint: ledger.configuration_fingerprint(),
            };
            let process:PodProcessBinding=serde_json::from_value(json!({"namespace":"awaken-system","name":"worker","uid":"pod-uid","node":"node-a","containers":[],"retained":true})).unwrap();
            let mut competing = stored.lock().unwrap().as_ref().unwrap()["data"].clone();
            if seal_wins {
                let mut state =
                    WorkerCreateLedgerState::decode(competing["ledger"].as_str().unwrap()).unwrap();
                state.seal_worker();
                competing["ledger"] = json!(state.encode().unwrap());
            } else {
                competing["worker_process_binding"] =
                    json!(serde_json::to_string(&process).unwrap());
            }
            *conflicting_data.lock().unwrap() = Some(competing.clone());
            conflict.store(true, Ordering::SeqCst);
            let result = if seal_wins {
                ledger.retain_process_binding(binding, &process).await
            } else {
                ledger.seal_unattempted_worker(binding).await
            };
            assert!(result.is_err(), "X1/X2 stale admission rejected");
            assert_eq!(
                stored.lock().unwrap().as_ref().unwrap()["data"],
                competing,
                "X1/X2 competing bytes retained"
            );
        }
    }

    #[tokio::test]
    async fn unattempted_worker_seal_is_exact_and_never_backfills_participants() {
        // C1 original fresh ledger + no process/tickets; C2 absent/legacy
        // ledger or a retained process/ticket. R1 C1 -> same Worker seal, exact
        // replay stutters and late binding/Create are refused. R2 C2 -> error,
        // original bytes unchanged. This is admission denial, not exit proof.
        for cause in ["empty", "bound", "ticket", "legacy", "missing"] {
            let Fixture {
                ledger,
                stored,
                calls,
                ..
            } = fixture(false);
            let (name, uid) = ledger
                .initialize("awaken-system", "worker", "pod-uid", "pod-uid")
                .await
                .unwrap();
            let binding = K8sLedgerBinding {
                worker_namespace: "awaken-system",
                worker_name: "worker",
                worker_uid: "pod-uid",
                worker_incarnation: "pod-uid",
                ledger_name: &name,
                ledger_uid: &uid,
                configuration_fingerprint: ledger.configuration_fingerprint(),
            };
            if cause == "bound" {
                let process:PodProcessBinding=serde_json::from_value(json!({"namespace":"awaken-system","name":"worker","uid":"pod-uid","node":"node-a","containers":[],"retained":true})).unwrap();
                ledger
                    .retain_process_binding(binding, &process)
                    .await
                    .unwrap();
            } else if cause == "ticket" {
                let request =
                    CreateTicketRequest::new("effect-a", "pods", "ns", "pod", "a".repeat(64))
                        .unwrap();
                ledger.admit_request(binding, &request).await.unwrap();
            } else if cause == "legacy" {
                let mut guard = stored.lock().unwrap();
                let object = guard.as_mut().unwrap();
                let encoded = object["data"]["ledger"].as_str().unwrap();
                let mut state: Value = serde_json::from_str(encoded).unwrap();
                state.as_object_mut().unwrap().remove("capture_policy");
                object["data"]["ledger"] = json!(state.to_string());
            } else if cause == "missing" {
                *stored.lock().unwrap() = None;
            }
            let before = stored.lock().unwrap().clone();
            let result = ledger.seal_unattempted_worker(binding).await;
            assert_eq!(result.is_ok(), cause == "empty", "R1/R2 {cause}");
            if cause == "empty" {
                let writes = calls
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|method| *method == "PUT")
                    .count();
                ledger.seal_unattempted_worker(binding).await.unwrap();
                assert_eq!(
                    calls
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|method| *method == "PUT")
                        .count(),
                    writes,
                    "R1 replay"
                );
                let request =
                    CreateTicketRequest::new("effect-a", "pods", "ns", "pod", "a".repeat(64))
                        .unwrap();
                assert_eq!(
                    ledger.admit_request(binding, &request).await.unwrap().0,
                    TicketAdmission::Sealed,
                    "R1 late Create"
                );
            } else {
                assert_eq!(*stored.lock().unwrap(), before, "R2 no backfill/mutation");
            }
        }
    }

    #[tokio::test]
    async fn sealed_worker_cannot_gain_its_first_registration_process_binding() {
        // Cause graph: C1 Worker seal wins the existing ConfigMap CAS; C2 a
        // first process-binding response arrives late; C3 binding was already
        // durable before seal. R1 C1+C2 -> reject, no binding/PUT; R2 C3 -> exact
        // replay remains readable and cannot replace the original. This is a
        // denial fence, not proof that an absent Worker never executed.
        for bound_first in [false, true] {
            let Fixture { ledger, calls, .. } = fixture(false);
            let (name, uid) = ledger
                .initialize("awaken-system", "worker", "pod-uid", "pod-uid")
                .await
                .unwrap();
            let binding = K8sLedgerBinding {
                worker_namespace: "awaken-system",
                worker_name: "worker",
                worker_uid: "pod-uid",
                worker_incarnation: "pod-uid",
                ledger_name: &name,
                ledger_uid: &uid,
                configuration_fingerprint: ledger.configuration_fingerprint(),
            };
            let process:PodProcessBinding=serde_json::from_value(json!({"namespace":"awaken-system","name":"worker","uid":"pod-uid","node":"node-a","containers":[],"retained":true})).unwrap();
            if bound_first {
                ledger
                    .retain_process_binding(binding, &process)
                    .await
                    .unwrap();
            }
            ledger.seal_worker(binding).await.unwrap();
            let writes = calls
                .lock()
                .unwrap()
                .iter()
                .filter(|method| *method == "PUT")
                .count();
            let result = ledger.retain_process_binding(binding, &process).await;
            assert_eq!(result.is_ok(), bound_first, "R1/R2");
            assert_eq!(
                calls
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|method| *method == "PUT")
                    .count(),
                writes,
                "R1/R2 no new binding after seal"
            );
            assert_eq!(
                ledger.read_process_binding(binding).await.unwrap(),
                bound_first.then_some(process),
                "R1/R2 original bytes only"
            );
        }
    }

    #[tokio::test]
    async fn registered_process_binding_is_one_immutable_ledger_participant() {
        // Causes: P1 exact retained Worker Pod process is observed before
        // Ready; P2 a retry presents identical or changed bytes; P3 a foreign
        // Pod UID is presented. Effects: E1 P1 persists once under the same
        // resourceVersion CAS as Create tickets; E2 an exact replay stutters;
        // E3 changed/foreign coordinates never replace the original. A later
        // process may read the original only through the Registry-frozen UID.
        let Fixture { ledger, calls, .. } = fixture(false);
        let (name, uid) = ledger
            .initialize("awaken-system", "worker", "pod-uid", "pod-uid")
            .await
            .unwrap();
        let binding = K8sLedgerBinding {
            worker_namespace: "awaken-system",
            worker_name: "worker",
            worker_uid: "pod-uid",
            worker_incarnation: "pod-uid",
            ledger_name: &name,
            ledger_uid: &uid,
            configuration_fingerprint: ledger.configuration_fingerprint(),
        };
        let process: PodProcessBinding = serde_json::from_value(json!({
            "namespace":"awaken-system", "name":"worker", "uid":"pod-uid",
            "node":"node-a", "containers":[], "retained":true
        }))
        .unwrap();
        ledger
            .retain_process_binding(binding, &process)
            .await
            .unwrap();
        assert_eq!(
            ledger.read_process_binding(binding).await.unwrap(),
            Some(process.clone()),
            "E1"
        );
        let writes = calls
            .lock()
            .unwrap()
            .iter()
            .filter(|method| *method == "PUT")
            .count();
        ledger
            .retain_process_binding(binding, &process)
            .await
            .unwrap();
        assert_eq!(
            calls
                .lock()
                .unwrap()
                .iter()
                .filter(|method| *method == "PUT")
                .count(),
            writes,
            "E2"
        );
        let changed: PodProcessBinding = serde_json::from_value(json!({
            "namespace":"awaken-system", "name":"worker", "uid":"pod-uid",
            "node":"node-b", "containers":[], "retained":true
        }))
        .unwrap();
        assert!(
            ledger
                .retain_process_binding(binding, &changed)
                .await
                .is_err(),
            "E3 changed node"
        );
        let foreign: PodProcessBinding = serde_json::from_value(json!({
            "namespace":"awaken-system", "name":"worker", "uid":"foreign-uid",
            "node":"node-a", "containers":[], "retained":true
        }))
        .unwrap();
        assert!(
            ledger
                .retain_process_binding(binding, &foreign)
                .await
                .is_err(),
            "E3 foreign UID"
        );
        assert_eq!(
            ledger.read_process_binding(binding).await.unwrap(),
            Some(process),
            "E3 original persists"
        );
    }
}
