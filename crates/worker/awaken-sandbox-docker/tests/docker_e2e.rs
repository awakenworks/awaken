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
    let after = readopted.spawn(cmd(&["true"])).await.expect("exec post-adopt");
    assert_eq!(after.wait().await.unwrap().code, Some(0));

    // Dispose reaps the container; status flips to Terminated.
    readopted.dispose().await.expect("dispose");
    assert_eq!(sandbox.status().await.unwrap(), SandboxStatus::Terminated);
}
