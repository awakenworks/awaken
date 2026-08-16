//! Live CNI-policy drift proof. Run only through the gated k3d suite.
#![cfg(feature = "k8s")]

use awaken_sandbox_container::ContainerRuntime;
use awaken_sandbox_container::k8s::K8sRuntime;
use k8s_openapi::api::networking::v1::NetworkPolicy;
use kube::api::{DeleteParams, Patch, PatchParams};

#[tokio::test]
async fn additive_sandbox_egress_drift_revokes_runtime_readiness() {
    if std::env::var("AWAKEN_K8S_E2E").as_deref() != Ok("1") {
        eprintln!("skipping: set AWAKEN_K8S_E2E=1 for the destructive drift proof");
        return;
    }
    /* FMECA / cause-effect graph: C1=canonical deny/open policies exist;
     * C2=an additional policy selects restricted Sandbox Pods and grants egress;
     * C3=the same K8sRuntime performs its next readiness probe. Effects:
     * E1=C1&&!C2 permits Ready; E2=C1+C2+C3 clears attested capability and
     * errors; E3=removing drift restores Ready. S=10/O=4/D=2, RPN=80.
     * Rules D1 C1&&!C2=>E1; D2 C1+C2+C3=>E2; D3 C1&&!C2+C3=>E3. */
    let runtime = K8sRuntime::connect("default", "127.0.0.1:1".parse().unwrap())
        .await
        .expect("D1 canonical policy graph");
    let policies = kube::Api::<NetworkPolicy>::namespaced(
        kube::Client::try_default().await.expect("kube client"),
        "default",
    );
    let drift = serde_json::json!({
        "apiVersion": "networking.k8s.io/v1",
        "kind": "NetworkPolicy",
        "metadata": { "name": "awaken-sandbox-test-illicit-egress" },
        "spec": {
            "podSelector": { "matchLabels": { "app": "awaken-sandbox" } },
            "policyTypes": ["Egress"],
            "egress": [{}]
        }
    });
    policies
        .patch(
            "awaken-sandbox-test-illicit-egress",
            &PatchParams::apply("awaken-policy-drift-test").force(),
            &Patch::Apply(drift),
        )
        .await
        .expect("install additive drift");

    let error = runtime
        .probe_ready()
        .await
        .expect_err("D2 additive egress must revoke readiness");
    assert!(error.to_string().contains("does not satisfy"), "D2");
    assert!(!runtime.enforces_network_none(), "D2 capability is cleared");

    policies
        .delete(
            "awaken-sandbox-test-illicit-egress",
            &DeleteParams::default(),
        )
        .await
        .expect("remove drift");
    for _ in 0..50 {
        if policies
            .get_opt("awaken-sandbox-test-illicit-egress")
            .await
            .expect("observe deletion")
            .is_none()
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    runtime
        .probe_ready()
        .await
        .expect("D3 restored policy graph");
    assert!(runtime.enforces_network_none(), "D3");
}
