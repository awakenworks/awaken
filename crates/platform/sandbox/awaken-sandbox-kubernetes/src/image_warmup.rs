// @formal surface requirement=runtime.permission.authority_never_widens
// @formal reason: Static owner: opt-in Kubernetes translation of existing immutable Environment warm
// @formal reason: demand into OpenKruise jobs using deployment namespace, node selector and existing
// @formal reason: pull-secret names only; no credential bytes, image catalog or Session authority are
// @formal reason: created. Dynamic path: validate digest and budget before effects, reuse only exact owned
// @formal reason: jobs, reject foreign objects, recover create conflicts or response loss by observation,
// @formal reason: and leave missing/forbidden infrastructure on the existing cold path without minting
// @formal reason: Ready capacity. Adjacent projection and supported-kube-transport decision-table tests
// @formal reason: cover this adapter boundary. This is a product-requirement classification, not a direct
// @formal reason: formal proof of Kubernetes or OpenKruise.
//! Node image preparation through Kubernetes' native DaemonSet controller.
//! Reuses the canonical realization fingerprint and create/recovery boundary.

use super::{K8sRuntime, api_not_found, backend, realization};
use awaken_container_contract::{K8sImagePrewarm, RuntimeError};
use k8s_openapi::api::apps::v1::DaemonSet;
use kube::api::{DeleteParams, ListParams, Preconditions};
use kube::{Api, ResourceExt};
use serde_json::{Value, json};
use std::collections::BTreeSet;

const OWNER_LABEL: &str = "awaken.dev/image-warmup";
const POLICY_LABEL: &str = "awaken.dev/image-warmup-policy";

fn policy_identity(policy: &K8sImagePrewarm, secrets: &[String]) -> String {
    let secrets = secrets.iter().collect::<BTreeSet<_>>();
    let bytes =
        serde_json::to_vec(&(&policy.node_selector, secrets)).expect("JSON values serialize");
    blake3::hash(&bytes).to_hex().as_str()[..32].to_owned()
}

fn image_daemonsets(
    namespace: &str,
    policy: &K8sImagePrewarm,
    secrets: &[String],
    images: &[String],
) -> Result<Vec<DaemonSet>, RuntimeError> {
    policy.validate().map_err(backend)?;
    let images = images.iter().collect::<BTreeSet<_>>();
    for image in &images {
        awaken_package_image::package_realization::immutable_image(image)
            .map_err(|error| backend(error.to_string()))?;
    }
    let policy_id = policy_identity(policy, secrets);
    let secrets = secrets.iter().collect::<BTreeSet<_>>();
    images.into_iter().take(policy.max_images).map(|image| {
        let identity = serde_json::to_vec(&(namespace, &policy_id, image)).expect("JSON values serialize");
        let name = format!("awaken-image-{}", &blake3::hash(&identity).to_hex().as_str()[..32]);
        let labels = json!({OWNER_LABEL:"true", POLICY_LABEL:policy_id, "awaken.dev/image-warmup-id":name});
        let mut spec = json!({
            "automountServiceAccountToken":false,
            "enableServiceLinks":false,
            "nodeSelector":policy.node_selector,
            "imagePullSecrets":secrets.iter().map(|name| json!({"name":name})).collect::<Vec<_>>(),
            "securityContext":{"runAsNonRoot":true,"runAsUser":10001,"runAsGroup":10001,"seccompProfile":{"type":"RuntimeDefault"}},
            "containers":[{"name":"image-cache","image":image,"imagePullPolicy":"IfNotPresent",
                "command":["/bin/sh","-c","exec sleep 2147483647"],
                "securityContext":{"allowPrivilegeEscalation":false,"readOnlyRootFilesystem":true,"capabilities":{"drop":["ALL"]}},
                "resources":{"requests":{"cpu":"1m","memory":"8Mi"},"limits":{"memory":"16Mi"}}}]
        });
        if policy.node_selector.is_empty() { spec.as_object_mut().unwrap().remove("nodeSelector"); }
        if secrets.is_empty() { spec.as_object_mut().unwrap().remove("imagePullSecrets"); }
        let mut holder: DaemonSet = serde_json::from_value(json!({"apiVersion":"apps/v1","kind":"DaemonSet",
            "metadata":{"namespace":namespace,"name":name,"labels":labels},
            "spec":{"selector":{"matchLabels":{"awaken.dev/image-warmup-id":name}},
                "template":{"metadata":{"labels":labels},"spec":spec}}})).map_err(backend)?;
        realization::stamp_realization(&mut holder)?;
        Ok(holder)
    }).collect()
}

fn projection(holder: &DaemonSet) -> Option<Value> {
    let spec = holder.spec.as_ref()?;
    let pod = spec.template.spec.as_ref()?;
    Some(
        json!({"selector":spec.selector,"labels":spec.template.metadata.as_ref()?.labels,
        "node_selector":pod.node_selector.clone().unwrap_or_default(),
        "pull_secrets":pod.image_pull_secrets.clone().unwrap_or_default(),
        "automount":pod.automount_service_account_token,"service_links":pod.enable_service_links,
        "security":pod.security_context,"host_network":pod.host_network.unwrap_or(false),
        "host_pid":pod.host_pid.unwrap_or(false),"host_ipc":pod.host_ipc.unwrap_or(false),
        "volumes":pod.volumes.clone().unwrap_or_default(),"init":pod.init_containers.clone().unwrap_or_default(),
        "containers":pod.containers.iter().map(|c| json!({"name":c.name,"image":c.image,"policy":c.image_pull_policy,
            "command":c.command,"args":c.args,"env":c.env,"env_from":c.env_from,
            "mounts":c.volume_mounts,"devices":c.volume_devices,"ports":c.ports,
            "lifecycle":c.lifecycle,"security":c.security_context,"resources":c.resources})).collect::<Vec<_>>() }),
    )
}

fn verify_holder(desired: &DaemonSet, observed: &DaemonSet) -> Result<(), RuntimeError> {
    realization::verify_realization(desired, observed, None)?;
    if observed.metadata.labels != desired.metadata.labels
        || projection(observed) != projection(desired)
    {
        return Err(backend(
            "image warmup DaemonSet has a foreign or conflicting projection",
        ));
    }
    Ok(())
}

impl K8sRuntime {
    pub fn with_image_prewarm(mut self, policy: K8sImagePrewarm) -> Result<Self, RuntimeError> {
        policy.validate().map_err(backend)?;
        self.image_prewarm = Some(policy);
        Ok(self)
    }

    pub(super) async fn reconcile_image_prewarm(
        &self,
        images: &[String],
    ) -> Result<(), RuntimeError> {
        let Some(policy) = &self.image_prewarm else {
            return Ok(());
        };
        let desired = image_daemonsets(&self.namespace, policy, &self.image_pull_secrets, images)?;
        let api: Api<DaemonSet> = Api::namespaced(self.clients.control.clone(), &self.namespace);
        for holder in &desired {
            match api.get(&holder.name_any()).await {
                Ok(existing) => verify_holder(holder, &existing)?,
                Err(error) if api_not_found(&error) => {
                    realization::create_or_verify_exact(&api, holder, verify_holder).await?;
                }
                Err(error) => return Err(backend(error)),
            }
        }
        // One bounded desired set from the current Environment snapshots. Delete
        // obsolete cache holders only in this exact policy scope, with the same
        // fingerprint/projection and UID+resourceVersion CAS fences.
        let scope = policy_identity(policy, &self.image_pull_secrets);
        let owned = api
            .list(
                &ListParams::default()
                    .labels(&format!("{OWNER_LABEL}=true,{POLICY_LABEL}={scope}")),
            )
            .await
            .map_err(backend)?;
        let names = desired
            .iter()
            .map(ResourceExt::name_any)
            .collect::<BTreeSet<_>>();
        for holder in owned.items {
            if names.contains(&holder.name_any()) {
                continue;
            }
            let image = holder
                .spec
                .as_ref()
                .and_then(|s| s.template.spec.as_ref())
                .and_then(|s| s.containers.first())
                .and_then(|c| c.image.clone())
                .ok_or_else(|| backend("cache holder has no exact image"))?;
            let expected =
                image_daemonsets(&self.namespace, policy, &self.image_pull_secrets, &[image])?
                    .remove(0);
            if expected.name_any() != holder.name_any() {
                return Err(backend("cache holder has a foreign identity"));
            }
            verify_holder(&expected, &holder)?;
            let uid = holder
                .metadata
                .uid
                .clone()
                .ok_or_else(|| backend("cache holder has no UID"))?;
            let version = holder
                .metadata
                .resource_version
                .clone()
                .ok_or_else(|| backend("cache holder has no resourceVersion"))?;
            api.delete(
                &holder.name_any(),
                &DeleteParams {
                    preconditions: Some(Preconditions {
                        uid: Some(uid),
                        resource_version: Some(version),
                    }),
                    ..Default::default()
                },
            )
            .await
            .map_err(backend)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{Request, Response};
    use kube::client::Body;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum FixtureScenario {
        Disabled,
        CreateSucceeds,
        ConcurrentOwnedCreate,
        ConcurrentForeignCreate,
        CreateResponseLost,
        GetForbidden,
        DemandRemoved,
        ProjectionTampered,
        DeleteConflict,
        UidMissing,
        ResourceVersionMissing,
        CreateResponsePending,
    }
    use FixtureScenario::*;

    fn image(letter: char) -> String {
        format!(
            "registry.test/sandbox@sha256:{}",
            letter.to_string().repeat(64)
        )
    }

    #[tokio::test]
    async fn disabled_create_replay_conflict_and_response_loss_use_one_holder() {
        // Transport decision table J0 disabled => no API; J1 absent/replay =>
        // one create; J2 own create race => canonical 409+GET; J3 foreign race
        // => reject; J4 lost create reply => error, later GET reuses exact object;
        // J5 forbidden GET => no mutation. All calls are native DaemonSets;
        // no Session Pod, PVC, custom CRD or readiness receipt is allocated.
        for scenario in [
            Disabled,
            CreateSucceeds,
            ConcurrentOwnedCreate,
            ConcurrentForeignCreate,
            CreateResponseLost,
            GetForbidden,
        ] {
            let (runtime, stored, calls) = fixture(scenario);
            let first = runtime.reconcile_image_prewarm(&[image('a')]).await;
            if scenario == Disabled {
                first.unwrap();
                runtime
                    .reconcile_image_prewarm(&["mutable:tag".into()])
                    .await
                    .unwrap();
                assert!(calls.lock().unwrap().is_empty());
                continue;
            }
            assert_eq!(
                first.is_ok(),
                scenario == CreateSucceeds || scenario == ConcurrentOwnedCreate,
                "{scenario:?}"
            );
            if scenario == CreateSucceeds
                || scenario == ConcurrentOwnedCreate
                || scenario == CreateResponseLost
            {
                runtime
                    .reconcile_image_prewarm(&[image('a')])
                    .await
                    .unwrap();
                assert_eq!(
                    calls
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|c| c.as_str() == "POST")
                        .count(),
                    1,
                    "one object {scenario:?}"
                );
                assert!(stored.lock().unwrap().is_some());
            }
            if scenario == GetForbidden {
                assert_eq!(*calls.lock().unwrap(), ["GET"]);
            }
        }
    }

    #[tokio::test]
    async fn obsolete_cache_cleanup_requires_exact_projection_and_cas() {
        // Causes C1 demand removed; C2 exact owned immutable projection;
        // C3 UID/resourceVersion present; C4 API delete CAS succeeds.
        // D1 all true => delete once; D2 changed projection => no delete;
        // D3 missing UID => no delete; D4 CAS conflict => error, no blind retry.
        for scenario in [
            DemandRemoved,
            ProjectionTampered,
            DeleteConflict,
            UidMissing,
            ResourceVersionMissing,
        ] {
            let (runtime, stored, calls) = fixture(scenario);
            runtime
                .reconcile_image_prewarm(&[image('a')])
                .await
                .unwrap();
            if scenario == ProjectionTampered {
                stored.lock().unwrap().as_mut().unwrap()["spec"]["template"]["spec"]["containers"]
                    [0]["image"] = json!(image('b'));
            }
            if scenario == UidMissing {
                stored.lock().unwrap().as_mut().unwrap()["metadata"]
                    .as_object_mut()
                    .unwrap()
                    .remove("uid");
            }
            if scenario == ResourceVersionMissing {
                stored.lock().unwrap().as_mut().unwrap()["metadata"]
                    .as_object_mut()
                    .unwrap()
                    .remove("resourceVersion");
            }
            let result = runtime.reconcile_image_prewarm(&[]).await;
            assert_eq!(result.is_ok(), scenario == DemandRemoved);
            assert_eq!(
                calls
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|c| c.as_str() == "DELETE")
                    .count(),
                usize::from(scenario == DemandRemoved || scenario == DeleteConflict)
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn canceled_create_is_recovered_by_the_same_persisted_holder() {
        // J6 POST already created the API object but its response never arrives
        // => caller cancellation leaves that controller-owned record intact.
        // The next Worker demand observation GETs the exact same identity and
        // projection; one POST total, then normal CAS cleanup on demand removal.
        // This is the surviving owner registered in cancel-safety fitness.
        let (runtime, stored, calls) = fixture(CreateResponsePending);
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(200),
                runtime.reconcile_image_prewarm(&[image('a')]),
            )
            .await
            .is_err()
        );
        assert!(stored.lock().unwrap().is_some());
        runtime
            .reconcile_image_prewarm(&[image('a')])
            .await
            .unwrap();
        assert_eq!(
            calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| call.as_str() == "POST")
                .count(),
            1
        );
        runtime.reconcile_image_prewarm(&[]).await.unwrap();
        assert!(stored.lock().unwrap().is_none());
    }

    type Objects = Arc<Mutex<Option<Value>>>;
    type Calls = Arc<Mutex<Vec<String>>>;
    fn fixture(scenario: FixtureScenario) -> (K8sRuntime, Objects, Calls) {
        let stored = Arc::new(Mutex::new(None::<Value>));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let (objects, requests) = (stored.clone(), calls.clone());
        let service = tower::service_fn(move |request: Request<Body>| {
            let (objects, requests) = (objects.clone(), requests.clone());
            async move {
                let collection = "/apis/apps/v1/namespaces/ns/daemonsets";
                assert!(request.uri().path().starts_with(collection));
                let list = request.method() == "GET" && request.uri().path() == collection;
                let method = request.method().to_string();
                requests
                    .lock()
                    .unwrap()
                    .push(if list { "LIST".into() } else { method.clone() });
                let bytes = request.into_body().collect_bytes().await.unwrap();
                let body: Value = if bytes.is_empty() {
                    Value::Null
                } else {
                    serde_json::from_slice(&bytes).unwrap()
                };
                let (status, value) = match method.as_str() {
                    "GET" if list => (
                        200,
                        json!({"apiVersion":"apps/v1","kind":"DaemonSetList","items":objects.lock().unwrap().iter().cloned().collect::<Vec<_>>() }),
                    ),
                    "GET" if scenario == GetForbidden => (403, Value::Null),
                    "GET" => objects
                        .lock()
                        .unwrap()
                        .clone()
                        .map(|v| (200, v))
                        .unwrap_or((404, Value::Null)),
                    "POST" => {
                        let mut value = body;
                        value["metadata"]["uid"] = json!("cache-uid");
                        value["metadata"]["resourceVersion"] = json!("41");
                        if scenario == ConcurrentForeignCreate {
                            value["metadata"]["labels"] = json!({"foreign":"true"});
                        }
                        *objects.lock().unwrap() = Some(value.clone());
                        if scenario == CreateResponsePending {
                            std::future::pending::<()>().await;
                        }
                        if scenario == CreateResponseLost {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::ConnectionReset,
                                "response lost",
                            ));
                        }
                        (
                            if scenario == ConcurrentOwnedCreate
                                || scenario == ConcurrentForeignCreate
                            {
                                409
                            } else {
                                201
                            },
                            value,
                        )
                    }
                    "DELETE" => {
                        assert_eq!(
                            body["preconditions"],
                            json!({"uid":"cache-uid","resourceVersion":"41"})
                        );
                        if scenario != DeleteConflict {
                            *objects.lock().unwrap() = None;
                        }
                        (
                            if scenario == DeleteConflict { 409 } else { 200 },
                            json!({"apiVersion":"v1","kind":"Status","status":"Success"}),
                        )
                    }
                    _ => panic!("unexpected mutation {method}"),
                };
                let value = if status >= 400 {
                    json!({"apiVersion":"v1","kind":"Status","status":"Failure","code":status,"message":"fixture","reason":"fixture"})
                } else {
                    value
                };
                Ok::<_, std::io::Error>(
                    Response::builder()
                        .status(status)
                        .body(Body::from(serde_json::to_vec(&value).unwrap()))
                        .unwrap(),
                )
            }
        });
        let mut runtime = K8sRuntime::for_test("127.0.0.1:1".parse().unwrap());
        runtime.namespace = "ns".into();
        runtime.clients.control = kube::Client::new(service, "ns");
        if scenario != Disabled {
            runtime = runtime.with_image_prewarm(Default::default()).unwrap();
        }
        (runtime, stored, calls)
    }

    #[test]
    fn immutable_deduplicated_bounded_jobs_keep_one_identity() {
        // I1 order/duplicates => same single holder; I2 changed namespace,
        // node selector, pull-secret or digest => new identity; I3 budget caps
        // holders; I4 sleep-only, no writable mounts/credentials/ServiceAccount.
        let policy = K8sImagePrewarm {
            max_images: 1,
            ..Default::default()
        };
        let a = image_daemonsets(
            "ns",
            &policy,
            &["pull".into()],
            &[image('b'), image('a'), image('a')],
        )
        .unwrap();
        let b = image_daemonsets(
            "ns",
            &policy,
            &["pull".into(), "pull".into()],
            &[image('a'), image('b')],
        )
        .unwrap();
        assert_eq!(a, b);
        assert_eq!(a.len(), 1);
        let pod = a[0].spec.as_ref().unwrap().template.spec.as_ref().unwrap();
        assert_eq!(
            pod.containers[0].image.as_deref(),
            Some(image('a').as_str())
        );
        assert!(pod.volumes.is_none());
        assert!(pod.containers[0].volume_mounts.is_none());
        assert_eq!(pod.automount_service_account_token, Some(false));
        assert_eq!(
            pod.containers[0].image_pull_policy.as_deref(),
            Some("IfNotPresent")
        );
        for (ns, selector, secret, reference) in [
            ("other", BTreeMap::new(), "pull", image('a')),
            (
                "ns",
                BTreeMap::from([("pool".into(), "other".into())]),
                "pull",
                image('a'),
            ),
            ("ns", BTreeMap::new(), "other", image('a')),
            ("ns", BTreeMap::new(), "pull", image('b')),
        ] {
            let holders = image_daemonsets(
                ns,
                &K8sImagePrewarm {
                    node_selector: selector,
                    ..policy.clone()
                },
                &[secret.into()],
                &[reference],
            )
            .unwrap();
            assert_ne!(a[0].name_any(), holders[0].name_any());
        }
    }

    #[test]
    fn invalid_images_foreign_and_api_defaulted_jobs() {
        // I5 mutable/invalid reference => reject; I6 matching stamp + projection
        // permits API defaults; forged stamp plus changed owner/image/command,
        // volumes, host networking or token mounting => reject before reuse.
        for image in [
            "sandbox:latest",
            "sandbox@sha256:short",
            "sandbox@sha256:gg",
        ] {
            assert!(image_daemonsets("ns", &Default::default(), &[], &[image.into()]).is_err());
        }
        let desired = image_daemonsets("ns", &Default::default(), &[], &[image('a')])
            .unwrap()
            .remove(0);
        let mut observed = desired.clone();
        observed.spec.as_mut().unwrap().revision_history_limit = Some(10);
        observed
            .spec
            .as_mut()
            .unwrap()
            .template
            .spec
            .as_mut()
            .unwrap()
            .dns_policy = Some("ClusterFirst".into());
        verify_holder(&desired, &observed).unwrap();
        for (pointer, value) in [
            ("/metadata/labels", json!({"foreign":"true"})),
            ("/spec/template/spec/containers/0/image", json!(image('b'))),
            (
                "/spec/template/spec/containers/0/command",
                json!(["/bin/evil"]),
            ),
            (
                "/spec/template/spec/volumes",
                json!([{"name":"host","hostPath":{"path":"/"}}]),
            ),
            ("/spec/template/spec/hostNetwork", json!(true)),
            (
                "/spec/template/spec/automountServiceAccountToken",
                json!(true),
            ),
        ] {
            let mut changed = serde_json::to_value(&observed).unwrap();
            // Optional fields are not serialized; add them to the exact spec.
            if let Some(slot) = changed.pointer_mut(pointer) {
                *slot = value;
            } else {
                let (parent, key) = pointer.rsplit_once('/').unwrap();
                changed
                    .pointer_mut(parent)
                    .unwrap()
                    .as_object_mut()
                    .unwrap()
                    .insert(key.into(), value);
            }
            let changed: DaemonSet = serde_json::from_value(changed).unwrap();
            assert!(verify_holder(&desired, &changed).is_err(), "{pointer}");
        }
    }
}
