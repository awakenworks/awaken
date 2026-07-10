//! Live-Docker e2e for `DockerSandboxProvider` (B-P5b). Gated on
//! `AWAKEN_TEST_DOCKER=1` (and a working docker daemon); skips otherwise. Proves the
//! load-bearing path: create a container sandbox, exec processes in it with correct
//! exit status, reconnect via a persisted handle (adopt), then dispose.

use awaken_provisioning_contract::{
    Command, IsolationClass, NetworkPolicy, SandboxProvider, SandboxSpec, SandboxStatus, Stdio,
};
use awaken_sandbox_docker::DockerSandboxProvider;

fn enabled() -> bool {
    std::env::var("AWAKEN_TEST_DOCKER").as_deref() == Ok("1")
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
async fn create_exec_adopt_dispose_over_real_docker() {
    if !enabled() {
        eprintln!("skip: AWAKEN_TEST_DOCKER != 1");
        return;
    }
    let provider = DockerSandboxProvider::new("alpine:3");

    // Container-isolated sandbox for this scope.
    let sandbox = provider.create(&spec("e2e-scope")).await.expect("create");
    assert_eq!(sandbox.status().await.unwrap(), SandboxStatus::Ready);

    // exec runs a process inside it with a correct exit code.
    let ok = sandbox.spawn(cmd(&["true"])).await.expect("spawn true");
    assert_eq!(ok.wait().await.unwrap().code, Some(0), "`true` exits 0");

    let fail = sandbox.spawn(cmd(&["false"])).await.expect("spawn false");
    assert_eq!(fail.wait().await.unwrap().code, Some(1), "`false` exits 1");

    // A real command with an argument.
    let sh = sandbox
        .spawn(cmd(&["sh", "-c", "exit 7"]))
        .await
        .expect("spawn sh");
    assert_eq!(sh.wait().await.unwrap().code, Some(7));

    // Persist the handle and reconnect (adopt) — the sandbox outlives its owner.
    let handle = sandbox.handle();
    assert_eq!(handle.provider_kind, "docker");
    let readopted = provider.adopt(&handle).await.expect("adopt");
    let after = readopted
        .spawn(cmd(&["true"]))
        .await
        .expect("exec post-adopt");
    assert_eq!(after.wait().await.unwrap().code, Some(0));

    // Dispose reaps the container; status flips to Terminated.
    readopted.dispose().await.expect("dispose");
    assert_eq!(sandbox.status().await.unwrap(), SandboxStatus::Terminated);
}

#[tokio::test]
async fn collects_and_reads_artifacts_from_the_outputs_path() {
    if !enabled() {
        eprintln!("skip: AWAKEN_TEST_DOCKER != 1");
        return;
    }
    let provider = DockerSandboxProvider::new("alpine:3");
    let sandbox = provider.create(&spec("artifacts")).await.expect("create");

    // A run produces a file under the environment's outputs root.
    let write = sandbox
        .spawn(cmd(&[
            "sh",
            "-c",
            "mkdir -p /workspace/out && printf 'hello-artifact' > /workspace/out/result.txt",
        ]))
        .await
        .expect("spawn write");
    assert_eq!(write.wait().await.unwrap().code, Some(0));

    // artifacts() content-addresses every file under outputs_path.
    let arts = sandbox.artifacts().await.expect("artifacts");
    assert_eq!(arts.len(), 1, "one produced artifact, got {arts:?}");
    assert!(arts[0].path.ends_with("result.txt"), "path: {}", arts[0].path);
    assert_eq!(arts[0].size_bytes, 14, "\"hello-artifact\" is 14 bytes");
    assert_eq!(arts[0].id, arts[0].content_hash, "id is the content hash");

    // read_artifact() returns the exact bytes, addressed by the content-hash id.
    let bytes = sandbox.read_artifact(&arts[0].id).await.expect("read_artifact");
    assert_eq!(bytes, b"hello-artifact");

    // An unknown id fails closed.
    assert!(sandbox.read_artifact("deadbeef").await.is_err());

    sandbox.dispose().await.expect("dispose");
}
