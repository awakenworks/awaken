//! Real Kubernetes end-to-end (k3d): launch a Session-owned Pod, exec a `busybox`
//! newline-wire agent into that same environment, and exchange the ACP wire over the
//! exec stdio channel. This proves the production `create`/`spawn_agent` path against
//! a live cluster (the multi-node cloud tier of ADR-0041/0056).
//!
//! Gated on the `k8s` feature AND `AWAKEN_K8S_E2E=1` with a reachable cluster
//! (KUBECONFIG pointing at it). It self-skips otherwise, so a machine without a
//! cluster still passes `cargo test`. The cluster + the `awaken-bb:1` fixture image
//! are set up by `scripts/e2e/k8s_container_e2e.sh`, which runs this test.
#![cfg(feature = "k8s")]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use awaken_provisioning_contract as pc;
use awaken_runtime_contract::llm::ToolCall;
use awaken_runtime_contract::tool::{ToolError, ToolExecutor};
use awaken_sandbox_container::k8s::K8sRuntime;
use awaken_sandbox_container::{ContainerProvider, ContainerSandbox, command_of};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct CredentialBroker(Mutex<Vec<u8>>);

#[async_trait]
impl pc::SecretBroker for CredentialBroker {
    async fn materialize(&self, _reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
        Ok(self.0.lock().unwrap().clone())
    }

    async fn materialize_process(&self, _reference: &str) -> Result<Vec<u8>, pc::SandboxError> {
        Err(pc::SandboxError::new("process secrets are not configured"))
    }

    async fn write_back(&self, _reference: &str, bytes: Vec<u8>) -> Result<(), pc::SandboxError> {
        *self.0.lock().unwrap() = bytes;
        Ok(())
    }
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

fn spec(scope: &str) -> pc::SandboxSpec {
    pc::SandboxSpec {
        scope: scope.into(),
        isolation: pc::IsolationClass::Container,
        mounts: Vec::new(),
        env: Vec::new(),
        packages: Default::default(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: "/mnt/session/outputs".into(),
        limits: pc::ResourceLimits::default(),
        lease_ttl_secs: None,
        extra: Some(serde_json::json!({ "command": session_argv(), "image": "awaken-bb:1" })),
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
        limits: pc::ResourceLimits::default(),
        lease_ttl_secs: None,
        extra: Some(serde_json::json!({ "command": session_argv(), "image": "awaken-bb:1" })),
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
        limits: pc::ResourceLimits::default(),
        lease_ttl_secs: None,
        extra: Some(serde_json::json!({ "command": session_argv(), "image": "awaken-bb:1" })),
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
        limits: pc::ResourceLimits::default(),
        lease_ttl_secs: None,
        extra: Some(serde_json::json!({ "command": session_argv(), "image": "awaken-bb:1" })),
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
        limits: Default::default(),
        lease_ttl_secs: None,
        extra: Some(serde_json::json!({
            "command": ["sh", "-c", format!("printf '%s' '{}' > /acp-config/auth.json; sleep 300", String::from_utf8_lossy(refreshed))],
            "image": "awaken-bb:1"
        })),
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

#[tokio::test]
async fn a_k8s_pod_rotates_and_persists_a_native_credential_file() {
    if std::env::var("AWAKEN_K8S_E2E").as_deref() != Ok("1")
        || !kubectl(&["get", "nodes"]).status.success()
    {
        eprintln!("skipping: set AWAKEN_K8S_E2E=1 with a reachable cluster to run");
        return;
    }
    let initial = br#"{"tokens":{"access_token":"old","refresh_token":"old"}}"#;
    let refreshed = br#"{"tokens":{"access_token":"new","refresh_token":"rotated"}}"#;
    let scope = format!("k8s-credential-{}", std::process::id());
    let broker = Arc::new(CredentialBroker(Mutex::new(initial.to_vec())));
    let runtime = K8sRuntime::connect("default", "127.0.0.1:1".parse().unwrap())
        .await
        .expect("connect to the cluster");
    let provider =
        ContainerProvider::new(Arc::new(runtime), "awaken-bb:1").with_secret_broker(broker.clone());
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
    assert_eq!(broker.0.lock().unwrap().as_slice(), refreshed);
    let secret_after = kubectl(&["get", "secret", &secret, "--ignore-not-found"]);
    assert!(
        String::from_utf8_lossy(&secret_after.stdout)
            .trim()
            .is_empty(),
        "credential Secret is deleted after harvest"
    );
}

#[tokio::test]
async fn a_pod_agent_speaks_the_wire_over_the_exec_channel() {
    // Cause/effect decision table — KRP1: C1 a Session-owned Pod is running;
    // C2 its opaque agent starts through attached exec; C3 runtime paths are not
    // part of the Pod's static environment. C1+C2+C3 => E1 the process receives
    // /workspace and the exact SandboxSpec output boundary, E2 it speaks on the
    // same stdio channel, and E3 disposal removes the Pod.
    if std::env::var("AWAKEN_K8S_E2E").as_deref() != Ok("1") {
        eprintln!("skipping: set AWAKEN_K8S_E2E=1 with a reachable cluster to run");
        return;
    }
    // The cluster must be reachable (kubeconfig points at it).
    if !kubectl(&["get", "nodes"]).status.success() {
        eprintln!("skipping: no reachable Kubernetes cluster");
        return;
    }

    let scope = format!("k8s-e2e-{}", std::process::id());
    let runtime = K8sRuntime::connect("default", "127.0.0.1:1".parse().unwrap())
        .await
        .expect("connect to the cluster");
    let provider = ContainerProvider::new(Arc::new(runtime), "awaken-bb:1");

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
    // removal makes the path absent without replacing the Pod.
    if std::env::var("AWAKEN_K8S_E2E").as_deref() != Ok("1") {
        eprintln!("skipping: set AWAKEN_K8S_E2E=1 with a reachable cluster to run");
        return;
    }
    if !kubectl(&["get", "nodes"]).status.success() {
        eprintln!("skipping: no reachable Kubernetes cluster");
        return;
    }

    let scope = format!("k8s-live-input-{}", std::process::id());
    let runtime = K8sRuntime::connect("default", "127.0.0.1:1".parse().unwrap())
        .await
        .expect("connect to the cluster");
    let provider = ContainerProvider::new(Arc::new(runtime), "awaken-bb:1");
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

    pc::Sandbox::dispose(&sandbox).await.unwrap();
}

#[tokio::test]
async fn inline_content_reaches_the_pod_as_a_configmap_volume() {
    if std::env::var("AWAKEN_K8S_E2E").as_deref() != Ok("1") {
        eprintln!("skipping: set AWAKEN_K8S_E2E=1 with a reachable cluster to run");
        return;
    }
    if !kubectl(&["get", "nodes"]).status.success() {
        eprintln!("skipping: no reachable Kubernetes cluster");
        return;
    }

    let scope = format!("k8s-cfg-{}", std::process::id());
    let marker = "inline-configmap-marker-42";

    let runtime = K8sRuntime::connect("default", "127.0.0.1:1".parse().unwrap())
        .await
        .expect("connect to the cluster");
    let provider = ContainerProvider::new(Arc::new(runtime), "awaken-bb:1");

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
    if std::env::var("AWAKEN_K8S_E2E").as_deref() != Ok("1") {
        eprintln!("skipping: set AWAKEN_K8S_E2E=1 with a reachable cluster to run");
        return;
    }
    if !kubectl(&["get", "nodes"]).status.success() {
        eprintln!("skipping: no reachable Kubernetes cluster");
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
        ContainerProvider::new(Arc::new(runtime), "awaken-bb:1").with_blob("blob-k8s", marker);

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
    if std::env::var("AWAKEN_K8S_E2E").as_deref() != Ok("1") {
        eprintln!("skipping: set AWAKEN_K8S_E2E=1 with a reachable cluster to run");
        return;
    }
    if !kubectl(&["get", "nodes"]).status.success() {
        eprintln!("skipping: no reachable Kubernetes cluster");
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
        ContainerProvider::new(Arc::new(runtime), "awaken-bb:1").with_blob("blob-bin", bytes);

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
    if std::env::var("AWAKEN_K8S_E2E").as_deref() != Ok("1") {
        eprintln!("skipping: set AWAKEN_K8S_E2E=1 with a reachable cluster to run");
        return;
    }
    if !kubectl(&["get", "nodes"]).status.success() {
        eprintln!("skipping: no reachable Kubernetes cluster");
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
    sandbox_spec.extra = Some(serde_json::json!({
        "command": session_argv(),
        "image": image,
    }));
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
