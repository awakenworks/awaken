//! Translate existing Environment warm demand into OpenKruise ImagePullJobs.
//! Kruise owns execution/status/TTL. This adapter owns no image cache or queue.

use std::collections::{BTreeMap, BTreeSet};

use kube::Api;
use kube::api::{ApiResource, DynamicObject, GroupVersionKind, PostParams};
use serde_json::{Value, json};

use super::{K8sRuntime, api_not_found, backend};
use crate::{K8sImagePullJobs, RuntimeError};

const OWNER_LABEL: &str = "awaken.dev/image-warmup";

fn image_jobs(
    namespace: &str,
    policy: &K8sImagePullJobs,
    secrets: &[String],
    images: &[String],
) -> Result<Vec<Value>, RuntimeError> {
    policy.validate().map_err(backend)?;
    let images = images.iter().collect::<BTreeSet<_>>();
    for image in &images {
        let digest = image.rsplit_once("@sha256:").map(|(_, digest)| digest);
        if !digest.is_some_and(|digest| {
            digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit())
        }) {
            return Err(backend(
                "node image warmup requires an immutable sha256 image reference",
            ));
        }
    }
    let secrets = secrets.iter().collect::<BTreeSet<_>>();
    Ok(images.into_iter().take(policy.max_images).map(|image| {
        let mut spec = json!({
            "image": image,
            "imagePullPolicy": "IfNotPresent",
            "parallelism": policy.parallelism,
            "selector": {"matchLabels": policy.node_selector},
            "pullSecrets": secrets,
            "completionPolicy": {"type": "Always", "activeDeadlineSeconds": 600, "ttlSecondsAfterFinished": 300},
            "pullPolicy": {"backoffLimit": 3, "timeoutSeconds": 300}
        });
        // Match the typed API omitempty shape before hashing/reusing a job.
        if policy.node_selector.is_empty() { spec.as_object_mut().unwrap().remove("selector"); }
        if secrets.is_empty() { spec.as_object_mut().unwrap().remove("pullSecrets"); }
        let identity = serde_json::to_vec(&(namespace, &spec)).expect("JSON values serialize");
        let name = format!("awaken-image-{}", &blake3::hash(&identity).to_hex().as_str()[..32]);
        json!({"apiVersion":"apps.kruise.io/v1alpha1", "kind":"ImagePullJob",
            "metadata":{"namespace":namespace,"name":name,"labels":{OWNER_LABEL:"true"}},"spec":spec})
    }).collect())
}

fn existing_job_admitted(
    labels: Option<&BTreeMap<String, String>>,
    spec: Option<&Value>,
    desired: &Value,
) -> bool {
    labels
        .and_then(|labels| labels.get(OWNER_LABEL))
        .is_some_and(|owner| owner == "true")
        && spec == desired.get("spec")
}

impl K8sRuntime {
    pub fn with_image_pull_jobs(mut self, policy: K8sImagePullJobs) -> Result<Self, RuntimeError> {
        policy.validate().map_err(backend)?;
        self.image_pull_jobs = Some(policy);
        Ok(self)
    }

    pub(super) async fn reconcile_image_pull_jobs(
        &self,
        images: &[String],
    ) -> Result<(), RuntimeError> {
        let Some(policy) = &self.image_pull_jobs else {
            return Ok(());
        };
        let jobs = image_jobs(&self.namespace, policy, &self.image_pull_secrets, images)?;
        let resource = ApiResource::from_gvk(&GroupVersionKind::gvk(
            "apps.kruise.io",
            "v1alpha1",
            "ImagePullJob",
        ));
        let api: Api<DynamicObject> =
            Api::namespaced_with(self.clients.control.clone(), &self.namespace, &resource);
        for job in jobs {
            let name = job["metadata"]["name"].as_str().expect("job has name");
            match api.get(name).await {
                Ok(existing) => {
                    if !existing_job_admitted(
                        existing.metadata.labels.as_ref(),
                        existing.data.get("spec"),
                        &job,
                    ) {
                        return Err(backend(
                            "image warmup job belongs to a foreign or conflicting owner",
                        ));
                    }
                    // Never reset an active/completed job; Kruise owns its retry
                    // and cleanup. A later reconciliation recreates after TTL.
                }
                Err(error) if api_not_found(&error) => {
                    let object: DynamicObject =
                        serde_json::from_value(job.clone()).map_err(backend)?;
                    match api.create(&PostParams::default(), &object).await {
                        Ok(_) => {}
                        Err(kube::Error::Api(error)) if error.code == 409 => {
                            let existing = api.get(name).await.map_err(backend)?;
                            if !existing_job_admitted(
                                existing.metadata.labels.as_ref(),
                                existing.data.get("spec"),
                                &job,
                            ) {
                                return Err(backend(
                                    "concurrent image warmup job has a foreign owner",
                                ));
                            }
                        }
                        Err(error) => return Err(backend(error)),
                    }
                }
                Err(error) => return Err(backend(error)),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{Request, Response};
    use kube::client::Body;
    use std::sync::{Arc, Mutex};

    fn image(letter: char) -> String {
        format!(
            "registry.test/sandbox@sha256:{}",
            letter.to_string().repeat(64)
        )
    }

    #[tokio::test]
    async fn disabled_create_replay_conflict_and_response_loss_use_one_job() {
        // Transport decision table: J0 disabled => no request, even for a tag;
        // J1 absent => GET+POST, replay => GET only; J2 concurrent own create
        // => 409+GET reuse; J3 concurrent foreign create => reject, no overwrite;
        // J4 POST response lost => error, later GET reuses the one actual object;
        // J5 GET forbidden => error with no mutation. Constraint: every request
        // targets ImagePullJob only; no Pod/PVC allocation or readiness receipt.
        for mode in 0..=5 {
            let stored = Arc::new(Mutex::new(None::<Value>));
            let calls = Arc::new(Mutex::new(Vec::<String>::new()));
            let (objects, requests) = (stored.clone(), calls.clone());
            let service = tower::service_fn(move |request: Request<Body>| {
                let (objects, requests) = (objects.clone(), requests.clone());
                async move {
                    assert!(
                        request.uri().path().starts_with(
                            "/apis/apps.kruise.io/v1alpha1/namespaces/ns/imagepulljobs"
                        )
                    );
                    let method = request.method().to_string();
                    requests.lock().unwrap().push(method.clone());
                    let bytes = request.into_body().collect_bytes().await.unwrap();
                    let body = if bytes.is_empty() {
                        Value::Null
                    } else {
                        serde_json::from_slice(&bytes).unwrap()
                    };
                    let (status, value) = match method.as_str() {
                        "GET" if mode == 5 => (403, Value::Null),
                        "GET" => match objects.lock().unwrap().clone() {
                            Some(value) => (200, value),
                            None => (404, Value::Null),
                        },
                        "POST" => {
                            let mut value = body;
                            if mode == 3 {
                                value["metadata"]["labels"] = json!({"foreign":"true"});
                            }
                            *objects.lock().unwrap() = Some(value.clone());
                            if mode == 4 {
                                return Err(std::io::Error::new(
                                    std::io::ErrorKind::ConnectionReset,
                                    "response lost",
                                ));
                            }
                            (if mode == 2 || mode == 3 { 409 } else { 201 }, value)
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
            if mode == 0 {
                runtime
                    .reconcile_image_pull_jobs(&["mutable:tag".into()])
                    .await
                    .unwrap();
                assert!(calls.lock().unwrap().is_empty(), "J0");
                continue;
            }
            let runtime = runtime.with_image_pull_jobs(Default::default()).unwrap();
            let first = runtime.reconcile_image_pull_jobs(&[image('a')]).await;
            assert_eq!(first.is_ok(), mode == 1 || mode == 2, "J{mode}");
            if mode == 1 || mode == 2 || mode == 4 {
                runtime
                    .reconcile_image_pull_jobs(&[image('a')])
                    .await
                    .unwrap();
                assert_eq!(
                    calls
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|method| method.as_str() == "POST")
                        .count(),
                    1,
                    "J{mode} one object"
                );
                let object = stored.lock().unwrap();
                assert_eq!(
                    object.as_ref().unwrap()["metadata"]["labels"][OWNER_LABEL],
                    "true"
                );
            }
            if mode == 5 {
                assert_eq!(*calls.lock().unwrap(), ["GET"], "J5");
            }
        }
    }

    #[test]
    fn immutable_deduplicated_bounded_jobs_keep_one_identity() {
        // Causes: C1 exact/changed image; C2 duplicate/order; C3 namespace,
        // selector or pull-secret differs; C4 budget. Effects: I1 same inputs
        // produce one stable job; I2 changed execution scope gets another name;
        // I3 bounded selected images; I4 no Session mounts/credentials bytes.
        let policy = K8sImagePullJobs {
            max_images: 1,
            ..Default::default()
        };
        let a = image_jobs(
            "ns",
            &policy,
            &["pull".into()],
            &[image('b'), image('a'), image('a')],
        )
        .unwrap();
        let b = image_jobs(
            "ns",
            &policy,
            &["pull".into(), "pull".into()],
            &[image('a'), image('b')],
        )
        .unwrap();
        assert_eq!(a, b, "I1 order/duplicates");
        assert_eq!(a.len(), 1, "I3");
        assert_eq!(a[0]["spec"]["image"], image('a'));
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
            let jobs = image_jobs(
                ns,
                &K8sImagePullJobs {
                    node_selector: selector,
                    ..policy.clone()
                },
                &[secret.into()],
                &[reference],
            )
            .unwrap();
            assert_ne!(a[0]["metadata"]["name"], jobs[0]["metadata"]["name"], "I2");
        }
        assert!(a[0]["spec"].get("volumes").is_none(), "I4");
        assert_eq!(a[0]["spec"]["imagePullPolicy"], "IfNotPresent");
    }

    #[test]
    fn invalid_images_and_foreign_jobs_fail_closed() {
        // Decision table I5 mutable/invalid reference => reject before API;
        // I6 owner label x exact spec: only both true permits job reuse.
        for image in [
            "sandbox:latest",
            "sandbox@sha256:short",
            "sandbox@sha256:gg",
        ] {
            assert!(
                image_jobs("ns", &Default::default(), &[], &[image.into()]).is_err(),
                "I5"
            );
        }
        let job = image_jobs("ns", &Default::default(), &[], &[image('a')])
            .unwrap()
            .remove(0);
        let labels = BTreeMap::from([(OWNER_LABEL.into(), "true".into())]);
        for owned in [false, true] {
            for exact in [false, true] {
                let observed = if exact {
                    job["spec"].clone()
                } else {
                    json!({"image":"foreign"})
                };
                assert_eq!(
                    existing_job_admitted(owned.then_some(&labels), Some(&observed), &job),
                    owned && exact,
                    "I6"
                );
            }
        }
    }
}
