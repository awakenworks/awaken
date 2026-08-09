//! Real Kubernetes end-to-end (k3d): launch a Session-owned Pod, exec a `busybox`
//! newline-wire agent into that same environment, and exchange the ACP wire over the
//! exec stdio channel. This proves the production `create`/`spawn_agent` path against
//! a live cluster (the multi-node cloud tier of ADR-0041/0056).
//!
//! Gated on the `k8s` feature AND `AWAKEN_K8S_E2E=1` with a reachable cluster
//! (KUBECONFIG pointing at it). It self-skips only when the gate is not requested;
//! once requested, missing infrastructure is a hard failure. The cluster + the
//! `awaken-bb:1` fixture image are set up by
//! `scripts/e2e/k8s_container_e2e.sh`, which runs this test.
#![cfg(feature = "k8s")]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use awaken_provisioning_contract as pc;
use awaken_runtime_contract::llm::ToolCall;
use awaken_runtime_contract::tool::{ToolError, ToolExecutor};
use awaken_sandbox_container::k8s::K8sRuntime;
use awaken_sandbox_container::{
    ContainerEnvironmentProvider, ContainerProvider, ContainerRuntime, ContainerSandbox,
    ContainerState, WarmContainerPool, command_of,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Default)]
struct CredentialBroker {
    current: Mutex<Vec<u8>>,
    reject_writeback: AtomicBool,
}

#[async_trait]
impl pc::SecretBroker for CredentialBroker {
    async fn materialize(&self, _reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
        Ok(self.current.lock().unwrap().clone())
    }

    async fn materialize_process(&self, _reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
        Err(pc::SandboxError::new("process secrets are not configured"))
    }

    async fn write_back(&self, _reference: &str, bytes: Vec<u8>) -> Result<(), pc::SandboxError> {
        if self.reject_writeback.load(Ordering::SeqCst) {
            return Err(pc::SandboxError::new("injected live broker rejection"));
        }
        *self.current.lock().unwrap() = bytes;
        Ok(())
    }
}

fn require_live_cluster() -> bool {
    if std::env::var("AWAKEN_K8S_E2E").as_deref() != Ok("1") {
        eprintln!("skipping: set AWAKEN_K8S_E2E=1 to require the live Kubernetes suite");
        return false;
    }
    assert!(
        kubectl(&["get", "nodes"]).status.success(),
        "AWAKEN_K8S_E2E=1 requires a reachable Kubernetes cluster"
    );
    true
}

/// A stdio agent fixture. Session-owned Kubernetes environments execute an agent via
/// the Pod exec subresource; they do not create a second, direct TCP control path.
fn agent_argv() -> Vec<String> {
    let script = "read _p; \
        printf '{\"type\":\"message\",\"text\":\"sandboxed reply:%s:%s\"}\\n' \
          \"$AWAKEN_PROJECT_DIR\" \"$AWAKEN_OUTPUTS_DIR\"; \
        printf '%s\\n' '{\"type\":\"turn_end\",\"reason\":\"natural_end\"}'";
    vec!["sh".into(), "-c".into(), script.into()]
}

fn session_argv() -> Vec<String> {
    vec!["sh".into(), "-c".into(), "sleep 300".into()]
}

fn fixture_image() -> String {
    std::env::var("AWAKEN_K8S_FIXTURE_IMAGE").unwrap_or_else(|_| "awaken-bb:1".to_string())
}

fn container_extra(command: Vec<String>, image: String) -> serde_json::Value {
    serde_json::json!({
        "command": command,
        "environment": { "kind": "image", "reference": image }
    })
}

fn spec(scope: &str) -> pc::SandboxSpec {
    pc::SandboxSpec {
        scope: scope.into(),
        isolation: pc::IsolationClass::Container,
        mounts: Vec::new(),
        env: Vec::new(),
        packages: Default::default(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: "/mnt/session/outputs".into(),
        requests: Default::default(),
        limits: pc::ResourceLimits::default(),
        lease_ttl_secs: None,
        extra: Some(container_extra(session_argv(), fixture_image())),
    }
}

/// A busybox fixture that reads the ConfigMap-projected file at `/acp-config/config.toml`
/// and echoes its contents back over the wire — so the host can prove the inline content
/// was materialized *inside the Pod*, not merely planned. Same newline-wire shape as
/// `agent_argv`, but the reply text is whatever the mounted file holds.
fn cat_mount_argv() -> Vec<String> {
    let script = "read _p; \
        printf '{\"type\":\"message\",\"text\":\"%s\"}\\n' \"$(cat /acp-config/config.toml)\"; \
        printf '%s\\n' '{\"type\":\"turn_end\",\"reason\":\"natural_end\"}'";
    vec!["sh".into(), "-c".into(), script.into()]
}

/// The e2e spec with one `Inline` mount the container tier realizes as a ConfigMap volume
/// (the codex `config.toml` / ADR-0038 resource path on the k8s tier).
fn inline_spec(scope: &str, marker: &str) -> pc::SandboxSpec {
    pc::SandboxSpec {
        scope: scope.into(),
        isolation: pc::IsolationClass::Container,
        mounts: vec![pc::MountRequirement {
            mount_id: "cfg".into(),
            source: pc::MountSource::Inline {
                contents: marker.into(),
            },
            mount_path: "/acp-config/config.toml".into(),
            access: pc::MountAccess::ReadOnly,
            lifetime: pc::MountLifetime::PerRun,
            required: true,
        }],
        env: Vec::new(),
        packages: Default::default(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: "/mnt/session/outputs".into(),
        requests: Default::default(),
        limits: pc::ResourceLimits::default(),
        lease_ttl_secs: None,
        extra: Some(container_extra(session_argv(), fixture_image())),
    }
}

fn managed_input_spec(scope: &str, path: &str, marker: &str) -> pc::SandboxSpec {
    pc::SandboxSpec {
        scope: scope.into(),
        isolation: pc::IsolationClass::Container,
        mounts: vec![pc::MountRequirement {
            mount_id: format!("managed-{marker}"),
            source: pc::MountSource::Inline {
                contents: marker.into(),
            },
            mount_path: path.into(),
            access: pc::MountAccess::ReadOnly,
            lifetime: pc::MountLifetime::Session,
            required: true,
        }],
        env: Vec::new(),
        packages: Default::default(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: "/mnt/session/outputs".into(),
        requests: Default::default(),
        limits: pc::ResourceLimits::default(),
        lease_ttl_secs: None,
        extra: Some(container_extra(session_argv(), fixture_image())),
    }
}

/// The e2e spec with one `File` mount whose bytes the provider resolves by id through the
/// injected `BlobSource` — then realizes as a ConfigMap volume just like inline content.
fn file_spec(scope: &str) -> pc::SandboxSpec {
    pc::SandboxSpec {
        scope: scope.into(),
        isolation: pc::IsolationClass::Container,
        mounts: vec![pc::MountRequirement {
            mount_id: "cfg".into(),
            source: pc::MountSource::File {
                file_id: "blob-k8s".into(),
                content_hash: None,
            },
            mount_path: "/acp-config/config.toml".into(),
            access: pc::MountAccess::ReadOnly,
            lifetime: pc::MountLifetime::PerRun,
            required: true,
        }],
        env: Vec::new(),
        packages: Default::default(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: "/mnt/session/outputs".into(),
        requests: Default::default(),
        limits: pc::ResourceLimits::default(),
        lease_ttl_secs: None,
        extra: Some(container_extra(session_argv(), fixture_image())),
    }
}

/// A busybox fixture that greps for an ASCII marker embedded in a BINARY file (the file
/// begins with non-UTF-8 bytes) — proving a `binaryData` ConfigMap reached the Pod intact.
fn grep_binary_argv() -> Vec<String> {
    let script = "read _p; \
        if grep -q binary-marker-ok /acp-config/config.toml; then M=binary-ok; else M=binary-missing; fi; \
        printf '{\"type\":\"message\",\"text\":\"%s\"}\\n' \"$M\"; \
        printf '%s\\n' '{\"type\":\"turn_end\",\"reason\":\"natural_end\"}'";
    vec!["sh".into(), "-c".into(), script.into()]
}

async fn exchange(
    sandbox: &ContainerSandbox<K8sRuntime>,
    argv: Vec<String>,
) -> Result<String, pc::SandboxError> {
    let process = sandbox
        .spawn_agent(pc::Command {
            argv,
            cwd: String::new(),
            env: Vec::new(),
            stdio: pc::Stdio::Piped,
        })
        .await?;
    let mut channel = process.channel;
    channel.write_all(b"hello\n").await.map_err(|error| {
        pc::SandboxError::new(format!("write to Kubernetes agent exec: {error}"))
    })?;
    channel
        .flush()
        .await
        .map_err(|error| pc::SandboxError::new(format!("flush Kubernetes agent exec: {error}")))?;

    let mut got = String::new();
    let mut buf = [0_u8; 512];
    for _ in 0..50 {
        match tokio::time::timeout(Duration::from_secs(2), channel.read(&mut buf)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => {
                got.push_str(&String::from_utf8_lossy(&buf[..n]));
                if got.contains("turn_end") {
                    break;
                }
            }
            Ok(Err(error)) => {
                return Err(pc::SandboxError::new(format!(
                    "read from Kubernetes agent exec: {error}"
                )));
            }
            Err(_) => break,
        }
    }
    let status = process.process.wait().await?;
    if status.code != Some(0) {
        return Err(pc::SandboxError::new(format!(
            "Kubernetes agent exec exited with {:?}",
            status.code
        )));
    }
    Ok(got)
}

/// The e2e spec with a `File` whose bytes are NON-UTF-8 — the k8s tier must realize it as a
/// ConfigMap `binaryData` entry (the text `data` path would reject the invalid UTF-8).
fn binary_file_spec(scope: &str) -> pc::SandboxSpec {
    pc::SandboxSpec {
        scope: scope.into(),
        isolation: pc::IsolationClass::Container,
        mounts: vec![pc::MountRequirement {
            mount_id: "cfg".into(),
            source: pc::MountSource::File {
                file_id: "blob-bin".into(),
                content_hash: None,
            },
            mount_path: "/acp-config/config.toml".into(),
            access: pc::MountAccess::ReadOnly,
            lifetime: pc::MountLifetime::PerRun,
            required: true,
        }],
        env: Vec::new(),
        packages: Default::default(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: "/mnt/session/outputs".into(),
        requests: Default::default(),
        limits: pc::ResourceLimits::default(),
        lease_ttl_secs: None,
        extra: Some(container_extra(session_argv(), fixture_image())),
    }
}

fn credential_spec(scope: &str, refreshed: &[u8]) -> pc::SandboxSpec {
    pc::SandboxSpec {
        scope: scope.into(),
        isolation: pc::IsolationClass::Container,
        mounts: vec![pc::MountRequirement {
            mount_id: "codex-auth".into(),
            source: pc::MountSource::Secret {
                reference: "credential://acp/native/codex".into(),
                content_hash: None,
            },
            mount_path: "/acp-config/auth.json".into(),
            access: pc::MountAccess::ReadWrite,
            lifetime: pc::MountLifetime::Durable,
            required: true,
        }],
        env: Vec::new(),
        packages: Default::default(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: "/mnt/session/outputs".into(),
        requests: Default::default(),
        limits: Default::default(),
        lease_ttl_secs: None,
        extra: Some(container_extra(
            vec![
                "sh".into(),
                "-c".into(),
                format!(
                    "printf '%s' '{}' > /acp-config/auth.json; sleep 300",
                    String::from_utf8_lossy(refreshed)
                ),
            ],
            fixture_image(),
        )),
    }
}

fn kubectl(args: &[&str]) -> std::process::Output {
    std::process::Command::new("kubectl")
        .args(args)
        .output()
        .expect("kubectl runs")
}

fn pod_of(sandbox: &ContainerSandbox<K8sRuntime>) -> String {
    pc::Sandbox::handle(sandbox)
        .extra
        .as_ref()
        .and_then(|extra| extra.get("container_id"))
        .and_then(serde_json::Value::as_str)
        .expect("the provider handle owns the exact Kubernetes Pod identity")
        .to_string()
}

fn cleanup_credential_pod(pod: &str) {
    let secret = format!("{pod}-credential-0");
    let _ = kubectl(&["delete", "pod", pod, "--ignore-not-found", "--wait=true"]);
    let _ = kubectl(&[
        "delete",
        "secret",
        &secret,
        "--ignore-not-found",
        "--wait=true",
    ]);
}

/// Kubernetes warm-capacity cause/effect design:
/// C1=reachable cluster, C2=mount-less exact shape, C3=target one, C4=Session
/// consumes the warm Pod, C5=capacity shutdown runs. E1=prewarm returns only
/// after the Pod is Ready, E2=Session receives that Pod without a cold create,
/// E3=shutdown deletes only unused capacity, E4=the active Session Pod remains
/// live until its own dispose, then reaches Gone.
/// Decision rule: (C1,C2,C3,C4,C5)->(E1,E2,E3,E4).
#[tokio::test]
async fn k8s_warm_capacity_reaches_ready_hands_out_and_drains_without_killing_session() {
    if !require_live_cluster() {
        return;
    }
    let namespace = std::env::var("AWAKEN_K8S_NAMESPACE").unwrap_or_else(|_| "default".into());
    let runtime = Arc::new(
        K8sRuntime::connect(&namespace, "127.0.0.1:1".parse().unwrap())
            .await
            .expect("connect to Kubernetes"),
    );
    let provider = Arc::new(ContainerProvider::new(runtime.clone(), fixture_image()));
    let pool = WarmContainerPool::new(provider, 1);
    let session_spec = spec(&format!("k8s-warm-capacity-{}", std::process::id()));

    assert_eq!(pool.prewarm(&session_spec, 1).await.unwrap(), 1, "E1");
    assert_eq!(pool.ready_len(&session_spec), 1);
    let environment = ContainerEnvironmentProvider::create_environment(&pool, &session_spec)
        .await
        .expect("E2 bind one Ready warm Pod to the Session");
    assert_eq!(pool.ready_len(&session_spec), 0, "E2 consumed capacity");
    assert_eq!(
        environment.status().await.unwrap(),
        pc::SandboxStatus::Ready
    );
    let pod = environment
        .handle()
        .extra
        .as_ref()
        .and_then(|extra| extra.get("container_id"))
        .and_then(serde_json::Value::as_str)
        .expect("warm Session owns an exact Pod")
        .to_string();

    pool.shutdown().await;
    assert_eq!(pool.ready_len(&session_spec), 0, "E3");
    assert_eq!(
        runtime.inspect(&pod).await.unwrap(),
        ContainerState::Running,
        "E3"
    );

    environment
        .dispose()
        .await
        .expect("dispose active Session Pod");
    assert_eq!(
        runtime.inspect(&pod).await.unwrap(),
        ContainerState::Gone,
        "E4"
    );
}

#[tokio::test]
async fn a_k8s_pod_rotates_and_persists_a_native_credential_file() {
    /* Credential-disposal FMECA rule KCF1. Causes: C1 a durable writable
     * credential is projected; C2 the agent replaces it; C3 remote read and
     * broker write both succeed. C1+C2+C3 => E1 persist the exact replacement,
     * E2 delete the Pod and projected Secret only after persistence. KCF2/KCF3
     * below cover each failed dependency and require preservation for retry.
     */
    if !require_live_cluster() {
        return;
    }
    let initial = br#"{"tokens":{"access_token":"old","refresh_token":"old"}}"#;
    let refreshed = br#"{"tokens":{"access_token":"new","refresh_token":"rotated"}}"#;
    let scope = format!("k8s-credential-{}", std::process::id());
    let broker = Arc::new(CredentialBroker {
        current: Mutex::new(initial.to_vec()),
        reject_writeback: AtomicBool::new(false),
    });
    let runtime = K8sRuntime::connect("default", "127.0.0.1:1".parse().unwrap())
        .await
        .expect("connect to the cluster");
    let provider = ContainerProvider::new(Arc::new(runtime), fixture_image())
        .with_secret_broker(broker.clone());
    let spec = credential_spec(&scope, refreshed);
    let sandbox = provider
        .create_container(&spec)
        .await
        .expect("create Pod with writable native credential");
    let pod = pod_of(&sandbox);
    let secret = format!("{pod}-credential-0");
    let mut ready = false;
    for _ in 0..120 {
        let phase = kubectl(&["get", "pod", &pod, "-o", "jsonpath={.status.phase}"]);
        if String::from_utf8_lossy(&phase.stdout) == "Running" {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(
        ready,
        "credential Pod reaches Running after its init container"
    );
    let process = sandbox
        .spawn_agent(pc::Command {
            argv: command_of(&spec),
            cwd: String::new(),
            env: Vec::new(),
            stdio: pc::Stdio::Piped,
        })
        .await
        .expect("start the credential-writing agent exec");
    tokio::time::sleep(Duration::from_millis(250)).await;
    process
        .process
        .signal(pc::Signal::Term)
        .await
        .expect("terminate the agent exec");
    pc::Sandbox::dispose(&sandbox)
        .await
        .expect("harvest the credential before deleting the Pod");
    assert_eq!(broker.current.lock().unwrap().as_slice(), refreshed);
    let secret_after = kubectl(&["get", "secret", &secret, "--ignore-not-found"]);
    assert!(
        String::from_utf8_lossy(&secret_after.stdout)
            .trim()
            .is_empty(),
        "credential Secret is deleted after harvest"
    );
}

#[tokio::test]
async fn live_credential_failures_preserve_the_k8s_environment_for_retry() {
    /* Credential-disposal FMECA cause/effect graph on the real apiserver:
     * C1 a durable writable credential exists; C2 remote `cat` exits 0;
     * C3 the broker accepts replacement material. Effects: E1 successful
     * harvest removes the Pod (covered by the preceding test); E2 !C2 or !C3
     * returns an error, keeps authoritative bytes unchanged, and preserves the
     * exact Pod for retry.
     *
     * | Rule | C2 remote read | C3 broker write | Result |
     * | KCF1 | success | success | E1 |
     * | KCF2 | failure | - | E2 |
     * | KCF3 | success | failure | E2 |
     */
    if !require_live_cluster() {
        return;
    }
    let initial = br#"{"tokens":{"access_token":"old"}}"#;
    let refreshed = br#"{"tokens":{"access_token":"rotated"}}"#;

    let read_scope = format!("k8s-credential-read-failure-{}", std::process::id());
    let read_broker = Arc::new(CredentialBroker {
        current: Mutex::new(initial.to_vec()),
        reject_writeback: AtomicBool::new(false),
    });
    let runtime = K8sRuntime::connect("default", "127.0.0.1:1".parse().unwrap())
        .await
        .expect("connect to the cluster");
    let provider = ContainerProvider::new(Arc::new(runtime), fixture_image())
        .with_secret_broker(read_broker.clone());
    let sandbox = provider
        .create_container(&credential_spec(&read_scope, refreshed))
        .await
        .expect("create credential Pod");
    let pod = pod_of(&sandbox);
    let removed = kubectl(&[
        "exec",
        &pod,
        "-c",
        "agent",
        "--",
        "rm",
        "-f",
        "/acp-config/auth.json",
    ]);
    assert!(removed.status.success(), "remove live credential fixture");
    let error = pc::Sandbox::dispose(&sandbox)
        .await
        .expect_err("KCF2 remote read failure must block disposal");
    assert!(error.to_string().contains("exit code"), "KCF2: {error}");
    assert_eq!(read_broker.current.lock().unwrap().as_slice(), initial);
    assert!(kubectl(&["get", "pod", &pod]).status.success(), "KCF2");
    cleanup_credential_pod(&pod);

    let write_scope = format!("k8s-credential-write-failure-{}", std::process::id());
    let write_broker = Arc::new(CredentialBroker {
        current: Mutex::new(initial.to_vec()),
        reject_writeback: AtomicBool::new(true),
    });
    let runtime = K8sRuntime::connect("default", "127.0.0.1:1".parse().unwrap())
        .await
        .expect("connect to the cluster");
    let provider = ContainerProvider::new(Arc::new(runtime), fixture_image())
        .with_secret_broker(write_broker.clone());
    let sandbox = provider
        .create_container(&credential_spec(&write_scope, refreshed))
        .await
        .expect("create credential Pod");
    let pod = pod_of(&sandbox);
    let error = pc::Sandbox::dispose(&sandbox)
        .await
        .expect_err("KCF3 broker rejection must block disposal");
    assert!(
        error.to_string().contains("broker rejection"),
        "KCF3: {error}"
    );
    assert_eq!(write_broker.current.lock().unwrap().as_slice(), initial);
    assert!(kubectl(&["get", "pod", &pod]).status.success(), "KCF3");
    cleanup_credential_pod(&pod);
}

#[tokio::test]
async fn a_pod_agent_speaks_the_wire_over_the_exec_channel() {
    // Cause/effect decision table — KRP1: C1 a Session-owned Pod is running;
    // C2 its opaque agent starts through attached exec; C3 runtime paths are not
    // part of the Pod's static environment. C1+C2+C3 => E1 the process receives
    // /workspace and the exact SandboxSpec output boundary, E2 it speaks on the
    // same stdio channel, and E3 disposal removes the Pod.
    if !require_live_cluster() {
        return;
    }

    let scope = format!("k8s-e2e-{}", std::process::id());
    let runtime = K8sRuntime::connect("default", "127.0.0.1:1".parse().unwrap())
        .await
        .expect("connect to the cluster");
    let provider = ContainerProvider::new(Arc::new(runtime), fixture_image());

    let sandbox = provider
        .create_container(&spec(&scope))
        .await
        .expect("create the agent Pod");
    let pod = pod_of(&sandbox);

    // The long-lived Session environment must be Running before its agent is exec'd.
    let ready = {
        let mut ok = false;
        for _ in 0..120 {
            let phase = kubectl(&["get", "pod", &pod, "-o", "jsonpath={.status.phase}"]);
            if String::from_utf8_lossy(&phase.stdout) == "Running" {
                ok = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        ok
    };

    assert!(ready, "the agent Pod must reach Running");
    let got = exchange(&sandbox, agent_argv())
        .await
        .expect("exchange ACP frames over the Kubernetes exec channel");
    let _ = pc::Sandbox::dispose(&sandbox).await;
    let _ = kubectl(&["delete", "pod", &pod, "--ignore-not-found", "--now"]);

    assert!(
        got.contains("sandboxed reply:/workspace:/mnt/session/outputs"),
        "the Pod agent must observe runtime paths over exec stdio: {got:?}"
    );
    assert!(got.contains("turn_end"), "the turn completed: {got:?}");
}

#[tokio::test]
async fn a_live_managed_file_is_replaceable_by_the_runtime_and_read_only_to_the_agent() {
    // Cause/effect decision table — KLI1: C1 a live K8s Session receives a
    // read-only File below the managed input root; C2 the runtime projector owns
    // the writable side of the shared volume; C3 the Agent owns neither projector
    // nor Kubernetes credentials. C1+C2+C3 => E1 attach becomes immediately
    // visible, E2 Agent writes fail while bytes stay unchanged, and E3 runtime
    // removal makes the path and now-empty parents absent without replacing the
    // Pod or deleting the shared projection root.
    if !require_live_cluster() {
        return;
    }

    let scope = format!("k8s-live-input-{}", std::process::id());
    let runtime = K8sRuntime::connect("default", "127.0.0.1:1".parse().unwrap())
        .await
        .expect("connect to the cluster");
    let provider = ContainerProvider::new(Arc::new(runtime), fixture_image());
    let sandbox = provider
        .create_container(&spec(&scope))
        .await
        .expect("create the agent Pod");
    let pod = pod_of(&sandbox);
    let path = "/mnt/session/uploads/awaken-design/current/index.html";
    let marker = "runtime-projected-generation-2";
    let requirement = pc::MountRequirement {
        mount_id: "current-index".into(),
        source: pc::MountSource::Inline {
            contents: marker.into(),
        },
        mount_path: path.into(),
        access: pc::MountAccess::ReadOnly,
        lifetime: pc::MountLifetime::Session,
        required: true,
    };

    pc::Sandbox::attach(&sandbox, requirement)
        .await
        .expect("project a live managed File");
    let read = kubectl(&["exec", &pod, "-c", "agent", "--", "cat", path]);
    assert!(read.status.success());
    assert_eq!(String::from_utf8_lossy(&read.stdout), marker);

    let write = kubectl(&[
        "exec",
        &pod,
        "-c",
        "agent",
        "--",
        "sh",
        "-c",
        "printf changed > /mnt/session/uploads/awaken-design/current/index.html",
    ]);
    assert!(
        !write.status.success(),
        "Agent must not mutate managed inputs"
    );
    let unchanged = kubectl(&["exec", &pod, "-c", "agent", "--", "cat", path]);
    assert_eq!(String::from_utf8_lossy(&unchanged.stdout), marker);

    sandbox
        .remove_live_input_path(path)
        .await
        .expect("remove the managed File through the projector");
    let absent = kubectl(&["exec", &pod, "-c", "agent", "--", "test", "!", "-e", path]);
    assert!(absent.status.success());
    let empty_parent_absent = kubectl(&[
        "exec",
        &pod,
        "-c",
        "agent",
        "--",
        "test",
        "!",
        "-e",
        "/mnt/session/uploads/awaken-design/current",
    ]);
    assert!(
        empty_parent_absent.status.success(),
        "hot removal must prune empty parents up to, but never including, the live-input root"
    );
    let root_retained = kubectl(&[
        "exec",
        &pod,
        "-c",
        "agent",
        "--",
        "test",
        "-d",
        "/mnt/session/uploads",
    ]);
    assert!(
        root_retained.status.success(),
        "projection root remains mounted"
    );

    pc::Sandbox::dispose(&sandbox).await.unwrap();
}

#[tokio::test]
async fn managed_manifest_recovery_reuses_the_pod_and_removes_obsolete_files() {
    /* Cause/effect recovery decision table — KLI2:
     * C1 a reachable cluster already has the exact Session Pod; C2 desired Managed
     * Files change from path/value A to path/value B; C3 all other environment facts
     * are unchanged. C1+C2+C3 => E1 create adopts the same Pod realization (no 500),
     * E2 B is projected before create returns, E3 obsolete A is absent, and E4 no
     * per-file ConfigMap exists. !C3 is covered by k8s_it K1 and must still fail
     * closed as a genuinely different realization.
     */
    if !require_live_cluster() {
        return;
    }

    let scope = format!("k8s-manifest-recovery-{}", std::process::id());
    let path_a = "/mnt/session/uploads/awaken-design/current/old.html";
    let path_b = "/mnt/session/uploads/awaken-design/current/index.html";
    let runtime = K8sRuntime::connect("default", "127.0.0.1:1".parse().unwrap())
        .await
        .expect("connect to the cluster");
    let provider = ContainerProvider::new(Arc::new(runtime), fixture_image());

    let first = provider
        .create_container(&managed_input_spec(&scope, path_a, "generation-a"))
        .await
        .expect("create the initial Session Pod and project A");
    let pod = pod_of(&first);
    let read_a = kubectl(&["exec", &pod, "-c", "agent", "--", "cat", path_a]);
    assert!(read_a.status.success());
    assert_eq!(String::from_utf8_lossy(&read_a.stdout), "generation-a");

    let recovered = provider
        .create_container(&managed_input_spec(&scope, path_b, "generation-b"))
        .await
        .expect("a changed Managed File manifest must reuse the stable Pod");
    assert_eq!(pod_of(&recovered), pod);
    let old_absent = kubectl(&["exec", &pod, "-c", "agent", "--", "test", "!", "-e", path_a]);
    assert!(old_absent.status.success());
    let read_b = kubectl(&["exec", &pod, "-c", "agent", "--", "cat", path_b]);
    assert!(read_b.status.success());
    assert_eq!(String::from_utf8_lossy(&read_b.stdout), "generation-b");

    let configmaps = kubectl(&[
        "get",
        "configmap",
        "-l",
        &format!("awaken-cfg-owner={pod}"),
        "-o",
        "name",
    ]);
    assert!(configmaps.status.success());
    assert!(configmaps.stdout.is_empty());

    pc::Sandbox::dispose(&recovered).await.unwrap();
}

#[tokio::test]
async fn inline_content_reaches_the_pod_as_a_configmap_volume() {
    /* Mount-realization FMECA rule KMR1. C1 inline UTF-8 bytes and a read-only
     * path are valid; C2 the Pod reaches Running. C1+C2 => E1 one ConfigMap
     * contains the exact text, E2 the agent reads it at the requested path,
     * E3 disposal reaps both Pod and ConfigMap. Invalid paths and create
     * rollback are owned by the pure realization decision table.
     */
    if !require_live_cluster() {
        return;
    }

    let scope = format!("k8s-cfg-{}", std::process::id());
    let marker = "inline-configmap-marker-42";

    let runtime = K8sRuntime::connect("default", "127.0.0.1:1".parse().unwrap())
        .await
        .expect("connect to the cluster");
    let provider = ContainerProvider::new(Arc::new(runtime), fixture_image());

    let sandbox = provider
        .create_container(&inline_spec(&scope, marker))
        .await
        .expect("create the agent Pod with a ConfigMap-backed inline mount");
    let pod = pod_of(&sandbox);

    // The ConfigMap must exist (created before the Pod, referenced as a volume).
    let cm = kubectl(&[
        "get",
        "configmap",
        &format!("{pod}-cfg-0"),
        "-o",
        "jsonpath={.data.content}",
    ]);
    let cm_data = String::from_utf8_lossy(&cm.stdout).to_string();

    let ready = {
        let mut ok = false;
        for _ in 0..120 {
            let phase = kubectl(&["get", "pod", &pod, "-o", "jsonpath={.status.phase}"]);
            if String::from_utf8_lossy(&phase.stdout) == "Running" {
                ok = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        ok
    };

    assert!(ready, "the agent Pod must reach Running");
    let got = exchange(&sandbox, cat_mount_argv())
        .await
        .expect("read the ConfigMap mount from the agent exec");
    let _ = pc::Sandbox::dispose(&sandbox).await;
    let _ = kubectl(&["delete", "pod", &pod, "--ignore-not-found", "--now"]);

    // The ConfigMap held the inline bytes verbatim.
    assert_eq!(
        cm_data, marker,
        "the ConfigMap must hold the inline content"
    );
    // The Pod `cat`ed the ConfigMap-projected file and sent it back — proof the inline
    // content was materialized *inside the Pod*, at the exact mount_path, and readable.
    assert!(
        got.contains(marker),
        "the ConfigMap-projected inline content must be readable at the mount_path in the Pod: {got:?}"
    );

    // dispose() reaps the ConfigMap too (best-effort label sweep); it must be gone.
    let after = kubectl(&[
        "get",
        "configmap",
        &format!("{pod}-cfg-0"),
        "--ignore-not-found",
    ]);
    assert!(
        String::from_utf8_lossy(&after.stdout).trim().is_empty(),
        "dispose must reap the inline-content ConfigMap"
    );
}

#[tokio::test]
async fn a_file_resolved_from_the_blob_source_reaches_the_pod() {
    /* By-reference mount FMECA rule KMR2. C1 the File id exists in the injected
     * BlobSource; C2 its bytes are UTF-8; C3 the Pod becomes runnable.
     * C1+C2+C3 => E1 resolve once through the canonical BlobSource, E2 project
     * the exact bytes, E3 make them readable only at the requested path. Missing
     * and corrupt content are covered at the provider boundary before mutation.
     */
    if !require_live_cluster() {
        return;
    }

    let scope = format!("k8s-file-{}", std::process::id());
    let marker = "file-via-blobsource-99";

    let runtime = K8sRuntime::connect("default", "127.0.0.1:1".parse().unwrap())
        .await
        .expect("connect to the cluster");
    // The provider resolves the `File` id `blob-k8s` from its seeded BlobSource, then the
    // k8s tier projects the resolved bytes as a ConfigMap — the by-reference content path.
    let provider =
        ContainerProvider::new(Arc::new(runtime), fixture_image()).with_blob("blob-k8s", marker);

    let sandbox = provider
        .create_container(&file_spec(&scope))
        .await
        .expect("create the agent Pod with a File mount resolved via BlobSource");
    let pod = pod_of(&sandbox);

    let ready = {
        let mut ok = false;
        for _ in 0..120 {
            let phase = kubectl(&["get", "pod", &pod, "-o", "jsonpath={.status.phase}"]);
            if String::from_utf8_lossy(&phase.stdout) == "Running" {
                ok = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        ok
    };

    assert!(ready, "the agent Pod must reach Running");
    let got = exchange(&sandbox, cat_mount_argv())
        .await
        .expect("read the resolved File mount from the agent exec");
    let _ = pc::Sandbox::dispose(&sandbox).await;
    let _ = kubectl(&["delete", "pod", &pod, "--ignore-not-found", "--now"]);

    // The Pod read the ConfigMap-projected file whose bytes the provider resolved from the
    // BlobSource by the File's content id — the full by-reference → ConfigMap → pod path.
    assert!(
        got.contains(marker),
        "a File resolved via BlobSource must reach the Pod at its mount_path: {got:?}"
    );
}

#[tokio::test]
async fn a_binary_file_reaches_the_pod_via_configmap_binary_data() {
    /* Binary mount FMECA rule KMR3. C1 the File id resolves; C2 bytes are not
     * valid UTF-8 but contain an exact marker; C3 the Pod becomes runnable.
     * C1+C2+C3 => E1 select ConfigMap binaryData rather than lossy text, E2
     * preserve all bytes, E3 expose the marker to the agent at the exact path.
     */
    if !require_live_cluster() {
        return;
    }

    let scope = format!("k8s-bin-{}", std::process::id());

    let runtime = K8sRuntime::connect("default", "127.0.0.1:1".parse().unwrap())
        .await
        .expect("connect to the cluster");
    // Non-UTF-8 bytes: a 0xff/0xfe prefix (so String::from_utf8 fails → binaryData) followed
    // by an ASCII marker the Pod greps for.
    let mut bytes = vec![0xffu8, 0xfe];
    bytes.extend_from_slice(b"binary-marker-ok");
    let provider =
        ContainerProvider::new(Arc::new(runtime), fixture_image()).with_blob("blob-bin", bytes);

    let sandbox = provider
        .create_container(&binary_file_spec(&scope))
        .await
        .expect("create the agent Pod with a binaryData ConfigMap mount");
    let pod = pod_of(&sandbox);

    let ready = {
        let mut ok = false;
        for _ in 0..120 {
            let phase = kubectl(&["get", "pod", &pod, "-o", "jsonpath={.status.phase}"]);
            if String::from_utf8_lossy(&phase.stdout) == "Running" {
                ok = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        ok
    };

    assert!(ready, "the agent Pod must reach Running");
    let got = exchange(&sandbox, grep_binary_argv())
        .await
        .expect("read the binaryData mount from the agent exec");
    let _ = pc::Sandbox::dispose(&sandbox).await;
    let _ = kubectl(&["delete", "pod", &pod, "--ignore-not-found", "--now"]);

    // The binary file (non-UTF-8 prefix + ASCII marker) reached the Pod intact via binaryData.
    assert!(
        got.contains("binary-ok"),
        "a binary File must reach the Pod via a binaryData ConfigMap: {got:?}"
    );
}

#[tokio::test]
async fn an_expired_hand_exec_is_safe_to_replace_inside_the_same_session_pod() {
    /*
     * Real Kubernetes Hand-expiry decision table. Causes: C1 a live Session Pod
     * executes bash through the production relay and Hand; C2 its attached-exec
     * process is killed before the next request; C3 the same dead executor is
     * invoked; C4 one replacement Hand is attached to the unchanged Pod.
     * Effects: E1 the initial call succeeds; E2 the dead channel is classified as
     * UnavailableBeforeDispatch (and therefore safe for the Session owner to
     * retry); E3 the replacement executes a second real bash; E4 the Session Pod
     * identity is unchanged and is disposed. Rule KHR1=C1+C2+C3+C4=>E1+E2+E3+E4.
     * Runtime-host's H1-H6 unit table separately proves that its canonical owner
     * performs exactly this one bounded replacement and never retries after dispatch.
     */
    if !require_live_cluster() {
        return;
    }

    let namespace = std::env::var("AWAKEN_K8S_NAMESPACE").unwrap_or_else(|_| "default".to_string());
    let image = std::env::var("AWAKEN_K8S_SESSION_IMAGE")
        .unwrap_or_else(|_| "awaken-sandbox:local".to_string());
    let scope = format!("k8s-hand-recovery-{}", std::process::id());
    let runtime = K8sRuntime::connect(&namespace, "127.0.0.1:1".parse().unwrap())
        .await
        .expect("connect to Kubernetes");
    let provider = ContainerProvider::new(Arc::new(runtime), image.clone());
    let mut sandbox_spec = spec(&scope);
    sandbox_spec.extra = Some(container_extra(session_argv(), image));
    let sandbox = provider
        .create_container(&sandbox_spec)
        .await
        .expect("create the Session Pod");
    let session_handle = pc::Sandbox::handle(&sandbox);

    let first = sandbox
        .spawn_agent(pc::Command {
            argv: vec![
                "/usr/local/bin/awaken-sandbox".into(),
                "hand".into(),
                "--stdio".into(),
            ],
            cwd: "/workspace".into(),
            env: Vec::new(),
            stdio: pc::Stdio::Piped,
        })
        .await
        .expect("attach the initial Hand");
    let first_executor = awaken_tool_relay::RemoteToolExecutor::new(first.channel)
        .with_operation_scope(pc::Sandbox::id(&sandbox));
    let before = first_executor
        .invoke(&ToolCall {
            call_id: "before-expiry".into(),
            tool_id: "bash".into(),
            arguments: serde_json::json!({"command": "printf before-expiry"}),
        })
        .await
        .expect("the initial Hand executes a real tool");
    assert!(before.text().contains("before-expiry"));
    first
        .process
        .signal(pc::Signal::Kill)
        .await
        .expect("kill the attached Hand exec");
    first.process.wait().await.expect("observe Hand exit");

    let error = first_executor
        .invoke(&ToolCall {
            call_id: "closed-channel".into(),
            tool_id: "bash".into(),
            arguments: serde_json::json!({"command": "printf must-not-run"}),
        })
        .await
        .expect_err("the expired channel cannot dispatch another call");
    assert!(matches!(error, ToolError::UnavailableBeforeDispatch(_)));

    let replacement = sandbox
        .spawn_agent(pc::Command {
            argv: vec![
                "/usr/local/bin/awaken-sandbox".into(),
                "hand".into(),
                "--stdio".into(),
            ],
            cwd: "/workspace".into(),
            env: Vec::new(),
            stdio: pc::Stdio::Piped,
        })
        .await
        .expect("attach one replacement Hand");
    let replacement_process = replacement.process;
    let replacement_executor = awaken_tool_relay::RemoteToolExecutor::new(replacement.channel)
        .with_operation_scope(pc::Sandbox::id(&sandbox));
    let after = replacement_executor
        .invoke(&ToolCall {
            call_id: "after-expiry".into(),
            tool_id: "bash".into(),
            arguments: serde_json::json!({"command": "printf after-expiry"}),
        })
        .await
        .expect("the replacement Hand executes a real tool");
    assert!(after.text().contains("after-expiry"));
    assert_eq!(pc::Sandbox::handle(&sandbox), session_handle);

    replacement_process
        .signal(pc::Signal::Term)
        .await
        .expect("stop the replacement Hand");
    replacement_process
        .wait()
        .await
        .expect("observe replacement exit");
    pc::Sandbox::dispose(&sandbox)
        .await
        .expect("dispose the Session Pod");
}
