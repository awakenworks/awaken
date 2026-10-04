//! Provider-owned, process-independent admission root for one Worker Pod.
//!
//! Only Coordinator holds a client with Create authority in the control
//! namespace. The Registry freezes the returned UID before Worker Ready; a
//! replacement process must never initialize a missing ledger to claim proof.

mod state;
pub use state::{
    CreateTicket, CreateTicketRequest, RequestJoin, TicketAdmission, TicketResult, TicketStatus,
    WorkerCreateLedgerState,
};

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::ConfigMap;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::PostParams;
use kube::{Api, Client, ResourceExt};

use super::PodProcessBinding;
use super::error::{api_conflict, backend};
use crate::RuntimeError;

const CONTRACT: &str = "awaken.dev/create-ledger-contract";
const PROVIDER: &str = "awaken.dev/create-ledger-provider";
const SANDBOX_NAMESPACE: &str = "awaken.dev/create-ledger-sandbox-namespace";
const WORKER_NAMESPACE: &str = "awaken.dev/create-ledger-worker-namespace";
const WORKER_NAME: &str = "awaken.dev/create-ledger-worker-name";
const WORKER_UID: &str = "awaken.dev/create-ledger-worker-uid";
const CONFIGURATION: &str = "awaken.dev/create-ledger-configuration";
const VERSION: &str = "v1";
const CAS_ATTEMPTS: usize = 8;

/// Registry-frozen witness plus the exact retained Worker Pod identity. The
/// provider verifies both before every ticket CAS; a stable ConfigMap name is
/// never accepted in place of the original UID.
#[derive(Clone, Copy)]
pub struct K8sLedgerBinding<'a> {
    pub worker_namespace: &'a str,
    pub worker_name: &'a str,
    pub worker_uid: &'a str,
    pub ledger_name: &'a str,
    pub ledger_uid: &'a str,
    pub configuration_fingerprint: &'a str,
}

/// One Kubernetes physical effect store, scoped by a retained Worker Pod UID.
#[derive(Clone)]
pub struct K8sWorkerCreateLedger {
    client: Client,
    control_namespace: String,
    sandbox_namespace: String,
    provider_id: String,
    configuration_fingerprint: String,
}

impl K8sWorkerCreateLedger {
    pub fn new(
        client: Client,
        control_namespace: impl Into<String>,
        sandbox_namespace: impl Into<String>,
        provider_id: impl Into<String>,
    ) -> Result<Self, RuntimeError> {
        let control_namespace = control_namespace.into();
        let sandbox_namespace = sandbox_namespace.into();
        let provider_id = provider_id.into();
        if [
            control_namespace.as_str(),
            sandbox_namespace.as_str(),
            provider_id.as_str(),
        ]
        .into_iter()
        .any(|part| part.trim().is_empty() || part.trim() != part)
            || control_namespace == sandbox_namespace
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
            sandbox_namespace,
            provider_id,
            configuration_fingerprint,
        })
    }

    pub fn control_namespace(&self) -> &str {
        &self.control_namespace
    }

    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }

    pub fn configuration_fingerprint(&self) -> &str {
        &self.configuration_fingerprint
    }

    fn ledger_name(worker_namespace: &str, worker_name: &str, worker_uid: &str) -> String {
        let identity = serde_json::to_vec(&(worker_namespace, worker_name, worker_uid))
            .expect("Worker Pod coordinate is serializable");
        let digest = blake3::hash(&identity).to_hex();
        format!("awaken-create-{}", &digest.as_str()[..32])
    }

    fn desired(&self, worker_namespace: &str, worker_name: &str, worker_uid: &str) -> ConfigMap {
        let name = Self::ledger_name(worker_namespace, worker_name, worker_uid);
        ConfigMap {
            metadata: ObjectMeta {
                name: Some(name),
                namespace: Some(self.control_namespace.clone()),
                annotations: Some(BTreeMap::from([
                    (CONTRACT.into(), VERSION.into()),
                    (PROVIDER.into(), self.provider_id.clone()),
                    (SANDBOX_NAMESPACE.into(), self.sandbox_namespace.clone()),
                    (WORKER_NAMESPACE.into(), worker_namespace.into()),
                    (WORKER_NAME.into(), worker_name.into()),
                    (WORKER_UID.into(), worker_uid.into()),
                    (CONFIGURATION.into(), self.configuration_fingerprint.clone()),
                ])),
                ..Default::default()
            },
            data: Some(BTreeMap::from([(
                "ledger".into(),
                WorkerCreateLedgerState::default()
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
                .is_some_and(|expected| {
                    observed
                        .metadata
                        .annotations
                        .as_ref()
                        .is_some_and(|actual| {
                            expected
                                .iter()
                                .all(|(key, value)| actual.get(key) == Some(value))
                        })
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

    /// Initialize only during verified Worker registration. A 409 reads the
    /// exact object rather than treating a stable name as proof. The Registry
    /// later rejects any same-incarnation retry that observes another UID.
    pub async fn initialize(
        &self,
        worker_namespace: &str,
        worker_name: &str,
        worker_uid: &str,
    ) -> Result<(String, String), RuntimeError> {
        if [worker_namespace, worker_name, worker_uid]
            .into_iter()
            .any(|part| part.trim().is_empty() || part.trim() != part)
            || worker_namespace != self.sandbox_namespace
        {
            return Err(backend(
                "Kubernetes Create ledger Worker Pod identity is invalid",
            ));
        }
        let desired = self.desired(worker_namespace, worker_name, worker_uid);
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

    fn expected(&self, binding: K8sLedgerBinding<'_>) -> Result<ConfigMap, RuntimeError> {
        if binding.worker_namespace != self.sandbox_namespace
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
            if !changed {
                return Ok(result);
            }
            object
                .data
                .get_or_insert_with(Default::default)
                .insert("ledger".into(), state.encode()?);
            match api
                .replace(binding.ledger_name, &PostParams::default(), &object)
                .await
            {
                Ok(updated) => {
                    if self.verify(&desired, &updated)? != binding.ledger_uid {
                        return Err(backend("Kubernetes Create ledger replacement changed UID"));
                    }
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
        {
            return Err(backend(
                "Worker Create ledger received an unretained or foreign Pod binding",
            ));
        }
        let encoded = serde_json::to_string(process).map_err(backend)?;
        self.cas_object(binding, |object, _| {
            let data = object.data.get_or_insert_with(Default::default);
            match data.get("worker_process_binding") {
                Some(existing) if existing == &encoded => Ok(((), false)),
                Some(_) => Err(backend(
                    "Worker Create ledger cannot replace its process binding",
                )),
                None => {
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
        {
            return Err(backend(
                "Worker Create ledger process binding no longer matches its Pod",
            ));
        }
        Ok(Some(process))
    }

    /// Only `Forward` means this exact request won durable admission; the
    /// caller must then own an independent API request lifetime.
    pub async fn admit_request(
        &self,
        binding: K8sLedgerBinding<'_>,
        request: &CreateTicketRequest,
    ) -> Result<TicketAdmission, RuntimeError> {
        self.mutate(binding, |state| {
            let admission = state.admit(request)?;
            let changed = admission == TicketAdmission::Forward;
            Ok((admission, changed))
        })
        .await
    }

    pub async fn finish_request(
        &self,
        binding: K8sLedgerBinding<'_>,
        request: &CreateTicketRequest,
        result: &TicketResult,
    ) -> Result<(), RuntimeError> {
        self.mutate(binding, |state| {
            let changed = state.finish(request, result.clone())?;
            Ok(((), changed))
        })
        .await
    }

    pub async fn seal_group_and_join(
        &self,
        binding: K8sLedgerBinding<'_>,
        group: &str,
    ) -> Result<RequestJoin, RuntimeError> {
        self.mutate(binding, |state| {
            let changed = state.seal_group(group)?;
            Ok(((), changed))
        })
        .await?;
        let (_, state) = self.load_exact(binding).await?;
        Ok(state.join_group(group))
    }

    pub async fn seal_worker(&self, binding: K8sLedgerBinding<'_>) -> Result<(), RuntimeError> {
        self.mutate(binding, |state| Ok(((), state.seal_worker())))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    }

    fn fixture(lose_first_response: bool) -> Fixture {
        let stored = Arc::new(Mutex::new(None::<Value>));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let lost = Arc::new(Mutex::new(false));
        let conflict = Arc::new(AtomicBool::new(false));
        let service = tower::service_fn({
            let (stored, calls, lost, conflict) = (
                stored.clone(),
                calls.clone(),
                lost.clone(),
                conflict.clone(),
            );
            move |request: Request<Body>| {
                let (stored, calls, lost, conflict) = (
                    stored.clone(),
                    calls.clone(),
                    lost.clone(),
                    conflict.clone(),
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
                            |value| (200, value),
                        ),
                        "PUT" => {
                            let mut current = stored.lock().unwrap();
                            let existing = current.as_mut().expect("PUT follows GET");
                            if conflict.swap(false, Ordering::SeqCst) {
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
                "provider-a",
            )
            .unwrap(),
            stored,
            calls,
            conflict,
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
                .initialize("awaken-system", "worker", "pod-uid")
                .await
                .is_err(),
            "J2 no response-only proof"
        );
        let (name, uid) = ledger
            .initialize("awaken-system", "worker", "pod-uid")
            .await
            .unwrap();
        assert!(name.starts_with("awaken-create-"));
        assert_eq!(uid, "uid-original", "J2 joined API UID");
        assert_eq!(*calls.lock().unwrap(), ["POST", "POST", "GET"]);

        stored.lock().unwrap().as_mut().unwrap()["metadata"]["uid"] = json!("uid-replaced");
        assert_eq!(
            ledger
                .initialize("awaken-system", "worker", "pod-uid")
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
                .initialize("awaken-system", "worker", "pod-uid")
                .await
                .is_err(),
            "J4 foreign provider"
        );
        stored.lock().unwrap().as_mut().unwrap()["metadata"]["annotations"][PROVIDER] =
            json!("provider-a");
        stored.lock().unwrap().as_mut().unwrap()["immutable"] = json!(true);
        assert!(
            ledger
                .initialize("awaken-system", "worker", "pod-uid")
                .await
                .is_err(),
            "J4 immutable ledger cannot accept ticket CAS"
        );
        stored.lock().unwrap().as_mut().unwrap()["immutable"] = json!(false);
        stored.lock().unwrap().as_mut().unwrap()["metadata"]["uid"] = json!("");
        assert!(
            ledger
                .initialize("awaken-system", "worker", "pod-uid")
                .await
                .is_err(),
            "J4 missing UID"
        );
    }

    #[tokio::test]
    async fn registration_ledger_rejects_unseparated_or_foreign_worker_scope() {
        // C1 control namespace equals Sandbox namespace -> constructor fails
        // before I/O; C2 Worker Pod names another Sandbox namespace -> no
        // Create request. Both effects keep the control ledger off the Worker
        // ServiceAccount's Sandbox mutation domain.
        let Fixture { ledger, calls, .. } = fixture(false);
        assert!(
            ledger
                .initialize("foreign", "worker", "pod-uid")
                .await
                .is_err()
        );
        assert!(calls.lock().unwrap().is_empty());
        assert!(
            K8sWorkerCreateLedger::new(
                ledger.client.clone(),
                "awaken-system",
                "awaken-system",
                "provider-a"
            )
            .is_err()
        );
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
        let Fixture {
            ledger,
            stored,
            calls,
            conflict,
        } = fixture(false);
        let (name, uid) = ledger
            .initialize("awaken-system", "worker", "pod-uid")
            .await
            .unwrap();
        let binding = K8sLedgerBinding {
            worker_namespace: "awaken-system",
            worker_name: "worker",
            worker_uid: "pod-uid",
            ledger_name: &name,
            ledger_uid: &uid,
            configuration_fingerprint: ledger.configuration_fingerprint(),
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
            ledger.admit_request(binding, &request).await.unwrap(),
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
            ledger.admit_request(binding, &request).await.unwrap(),
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
            .initialize("awaken-system", "worker", "pod-uid")
            .await
            .unwrap();
        let binding = K8sLedgerBinding {
            worker_namespace: "awaken-system",
            worker_name: "worker",
            worker_uid: "pod-uid",
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
