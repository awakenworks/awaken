//! Provider-owned, process-independent admission root for one Worker Pod.
//!
//! Only Coordinator holds a client with Create authority in the control
//! namespace. The Registry freezes the returned UID before Worker Ready; a
//! replacement process must never initialize a missing ledger to claim proof.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::ConfigMap;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::PostParams;
use kube::{Api, Client, ResourceExt};

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

/// One Kubernetes physical effect store, scoped by a retained Worker Pod UID.
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
                r#"{"version":1,"phase":"open","tickets":{}}"#.into(),
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{Request, Response};
    use kube::client::Body;
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};

    fn fixture(
        lose_first_response: bool,
    ) -> (
        K8sWorkerCreateLedger,
        Arc<Mutex<Option<Value>>>,
        Arc<Mutex<Vec<String>>>,
    ) {
        let stored = Arc::new(Mutex::new(None::<Value>));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let lost = Arc::new(Mutex::new(false));
        let service = tower::service_fn({
            let (stored, calls, lost) = (stored.clone(), calls.clone(), lost.clone());
            move |request: Request<Body>| {
                let (stored, calls, lost) = (stored.clone(), calls.clone(), lost.clone());
                async move {
                    let path = request.uri().path().to_owned();
                    assert!(path.starts_with("/api/v1/namespaces/awaken-control/configmaps"));
                    let method = request.method().as_str().to_owned();
                    calls.lock().unwrap().push(method.clone());
                    let bytes = request.into_body().collect_bytes().await.unwrap();
                    let (status, body) = match method.as_str() {
                        "POST" => {
                            let mut guard = stored.lock().unwrap();
                            if guard.is_none() {
                                let mut value: Value = serde_json::from_slice(&bytes).unwrap();
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
        (
            K8sWorkerCreateLedger::new(
                Client::new(service, "awaken-control"),
                "awaken-control",
                "awaken-system",
                "provider-a",
            )
            .unwrap(),
            stored,
            calls,
        )
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
        let (ledger, stored, calls) = fixture(true);
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
        let (ledger, _, calls) = fixture(false);
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
}
