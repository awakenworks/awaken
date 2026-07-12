//! Live-k3d e2e for `K8sSandboxProvider` (B-P5a). Gated on `AWAKEN_TEST_K8S_CONTEXT`
//! (a reachable kubectl context, e.g. `k3d-awaken-test`); skips otherwise. Proves the
//! declarative Pod path: apply a Pod sandbox, exec processes with correct exit
//! status, reconnect via a persisted handle (adopt), then dispose.

use awaken_provisioning_contract::{
    Command, IsolationClass, NetworkPolicy, SandboxProvider, SandboxSpec, SandboxStatus, Stdio,
};
use awaken_sandbox_k8s::K8sSandboxProvider;

fn context() -> Option<String> {
    std::env::var("AWAKEN_TEST_K8S_CONTEXT").ok()
}

fn spec(scope: &str) -> SandboxSpec {
    SandboxSpec {
        scope: scope.into(),
        isolation: IsolationClass::Container,
        mounts: Vec::new(),
        env: Vec::new(),
        network: NetworkPolicy::Unrestricted,
        outputs_path: "/workspace/out".into(),
        limits: Default::default(),
        lease_ttl_secs: None,
        extra: None,
    }
}

fn cmd(argv: &[&str]) -> Command {
    Command {
        argv: argv.iter().map(|s| s.to_string()).collect(),
        cwd: String::new(),
        env: Vec::new(),
        stdio: Stdio::Null,
    }
}

#[tokio::test]
async fn create_exec_adopt_dispose_over_real_k8s() {
    let Some(ctx) = context() else {
        eprintln!("skip: AWAKEN_TEST_K8S_CONTEXT unset");
        return;
    };
    let provider = K8sSandboxProvider::new("alpine:3", ctx, "default");

    // Declarative Pod sandbox for this scope.
    let sandbox = provider.create(&spec("k8se2e")).await.expect("create");
    assert_eq!(sandbox.status().await.unwrap(), SandboxStatus::Ready);

    // exec runs processes in the Pod with correct exit codes.
    let ok = sandbox.spawn(cmd(&["true"])).await.expect("spawn true");
    assert_eq!(ok.wait().await.unwrap().code, Some(0));
    let sh = sandbox
        .spawn(cmd(&["sh", "-c", "exit 7"]))
        .await
        .expect("spawn sh");
    assert_eq!(sh.wait().await.unwrap().code, Some(7));

    // Persist the handle and reconnect (adopt) — the Pod outlives its owner.
    let handle = sandbox.handle();
    assert_eq!(handle.provider_kind, "k8s");
    let readopted = provider.adopt(&handle).await.expect("adopt");
    let after = readopted
        .spawn(cmd(&["true"]))
        .await
        .expect("exec post-adopt");
    assert_eq!(after.wait().await.unwrap().code, Some(0));

    // Dispose reaps the Pod.
    readopted.dispose().await.expect("dispose");
}

#[tokio::test]
async fn collects_and_reads_artifacts_over_real_k8s() {
    let Some(ctx) = context() else {
        eprintln!("skip: AWAKEN_TEST_K8S_CONTEXT unset");
        return;
    };
    let provider = K8sSandboxProvider::new("alpine:3", ctx, "default");
    let sandbox = provider.create(&spec("k8sart")).await.expect("create");

    let write = sandbox
        .spawn(cmd(&[
            "sh",
            "-c",
            "mkdir -p /workspace/out && printf 'hello-artifact' > /workspace/out/result.txt",
        ]))
        .await
        .expect("spawn write");
    assert_eq!(write.wait().await.unwrap().code, Some(0));

    let arts = sandbox.artifacts().await.expect("artifacts");
    assert_eq!(arts.len(), 1, "one produced artifact, got {arts:?}");
    assert!(
        arts[0].path.ends_with("result.txt"),
        "path: {}",
        arts[0].path
    );
    assert_eq!(arts[0].size_bytes, 14);
    assert_eq!(arts[0].id, arts[0].content_hash);

    let bytes = sandbox
        .read_artifact(&arts[0].id)
        .await
        .expect("read_artifact");
    assert_eq!(bytes, b"hello-artifact");
    assert!(sandbox.read_artifact("deadbeef").await.is_err());

    sandbox.dispose().await.expect("dispose");
}
