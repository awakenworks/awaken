//! Recovery uses the existing scripted API, metadata CAS and readiness policy.
use super::*;
use crate::{k8s_runtime_id, pod_name, realization, restore};
use pc::test_support::{EffectAuthority, FenceTamper as _};
use std::sync::Mutex;
use std::time::Duration;

#[tokio::test]
async fn exact_recovery_refuses_stale_read_retry_or_ready_target_replacement() {
    // C1 retained, fully projected target; C2 response cut is first GET,
    // owner-CAS conflict GET or final readiness GET; C3 grant is revoked,
    // replaced, expired or renewed; C4 Ready returns a different UID.
    // R1 live unchanged/renewed => same UID and no Create; R2 stale first GET
    // => zero writes; R3 stale owner retry => no next PUT; R4 stale Ready or
    // replacement => no positive receipt. Every row retains the original Pod
    // and projected data. The common convergence edge is shared with fresh
    // acquisition; real Kubernetes acquisition remains a separate live gate.
    let rows = ["lookup", "ready"]
        .into_iter()
        .flat_map(|cut| ["revoke", "replace", "expire", "renew"].map(move |change| (cut, change)))
        .chain([("owner-retry", "revoke"), ("ready-replacement", "renew")]);
    for (cut, change) in rows {
        let mut runtime = K8sRuntime::for_test("127.0.0.1:9000".parse().unwrap());
        let plan = awaken_sandbox_container::test_support::container_plan();
        let fp = awaken_sandbox_container::engine::restoration_plan_fingerprint(&plan);
        let evidence = pc::SandboxRestorationEvidence::from_exact_parts(
            "effect",
            "generation",
            "checkpoint",
            "digest",
            "spec",
            "exclusions",
        )
        .unwrap();
        let (fence, _) = pc::test_support::fixture_effect("effect");
        // A renewal requires headroom below the fixture max-expiry sentinel.
        let fence = fence.with_persisted(|w| w.expires_at_unix_ms -= 1).unwrap();
        let authority = Arc::new(EffectAuthority::new(&fence));
        let id = "restore-recovery";
        let runtime_id = k8s_runtime_id(id).unwrap();
        let mut pod = runtime.pod(&runtime_id, &plan);
        awaken_package_image::package_realization::stamp_sandbox_release_annotations(
            &mut pod,
            id,
            &plan.image,
        );
        realization::stamp_restoration(&mut pod, &evidence);
        realization::stamp_restoration_plan(&mut pod, &fp);
        declare(&mut pod, &runtime_id, &plan).unwrap();
        realization::stamp_pod_realization(&mut pod).unwrap();
        pod.metadata.uid = Some("original-pod".into());
        pod.metadata.resource_version = Some("1".into());
        pod.metadata.labels.as_mut().unwrap().insert(
            awaken_sandbox_container::engine::RUNTIME_OWNER_LABEL.into(),
            "previous-owner".into(),
        );
        let projected = KubernetesProjectedParticipants::new(
            "default".into(),
            "original-pod".into(),
            vec![],
            None,
            Some((evidence.clone(), fp.clone())),
        )
        .unwrap();
        pod.metadata
            .annotations
            .as_mut()
            .unwrap()
            .insert(RESULTS.into(), serde_json::to_string(&projected).unwrap());
        let statuses: Vec<_> = pod.spec.as_ref().unwrap().containers.iter().map(|c| json!({
            "name":c.name,"image":"img:latest","imageID":"sha256:fixture","ready":true,"restartCount":0,"state":{"running":{}}
        })).collect();
        pod.status = Some(
            serde_json::from_value(json!({"phase":"Running","containerStatuses":statuses}))
                .unwrap(),
        );
        let path = format!("/api/v1/namespaces/default/pods/{}", pod_name(&runtime_id));
        let objects: ProjectionObjects = Arc::new(Mutex::new(
            [(path.clone(), serde_json::to_value(&pod).unwrap())].into(),
        ));
        let writes = Arc::new(AtomicUsize::new(0));
        let faults: PublicationFaults = Arc::new(Mutex::new(Default::default()));
        if cut == "ready" || cut == "ready-replacement" {
            faults
                .lock()
                .unwrap()
                .push_back(PublicationFault::AfterGet(Arc::new(|_| {})));
        } else if cut == "owner-retry" {
            faults.lock().unwrap().push_back(PublicationFault::Status);
        }
        let changing = authority.clone();
        faults
            .lock()
            .unwrap()
            .push_back(PublicationFault::AfterGet(Arc::new(move |body| {
                if cut == "ready-replacement" {
                    body["metadata"]["uid"] = "replacement-pod".into();
                    return;
                }
                let mut current = changing
                    .current
                    .lock()
                    .unwrap_or_else(|error| panic!("{error}"));
                match change {
                    "revoke" => *current = None,
                    "replace" => current.as_mut().unwrap().0.reassign(),
                    "expire" => current.as_mut().unwrap().1 = Duration::ZERO,
                    "renew" => current.as_mut().unwrap().0.renew(),
                    _ => unreachable!(),
                }
            })));
        runtime.clients.control = projected_client(
            objects.clone(),
            Default::default(),
            writes.clone(),
            Default::default(),
            Default::default(),
            faults,
            Default::default(),
        );
        let result = restore::recover(
            &runtime,
            id,
            &plan,
            &fp,
            &evidence,
            pc::SandboxEffect::Fenced {
                fence: &fence,
                authority: authority.as_ref(),
            },
        )
        .await;
        assert_eq!(
            result.is_ok(),
            change == "renew" && cut != "ready-replacement",
            "{cut}/{change}: {result:?}"
        );
        if let Ok(Some(target)) = result {
            assert_eq!(target.container_id, pod_name(&runtime_id));
        }
        assert_eq!(
            writes.load(Ordering::SeqCst),
            usize::from(cut != "lookup" || change == "renew"),
            "R2/R3 no subsequent writes: {cut}/{change}"
        );
        let retained = objects.lock().unwrap();
        assert_eq!(
            retained[&path]["metadata"]["uid"], "original-pod",
            "original target retained"
        );
        assert_eq!(
            retained[&path]["metadata"]["annotations"][RESULTS],
            pod.metadata.annotations.as_ref().unwrap()[RESULTS]
        );
    }
}
