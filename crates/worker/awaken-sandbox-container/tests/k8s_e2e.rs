//! Real Kubernetes end-to-end (k3d): launch a process-as-container agent Pod running a
//! `busybox` newline-wire fixture, port-forward its agent port, and exchange the ACP
//! wire — proving the `K8sRuntime` create/inspect/open_channel path against a live
//! cluster (the multi-node cloud tier of ADR-0041/0056).
//!
//! Gated on the `k8s` feature AND `AWAKEN_K8S_E2E=1` with a reachable cluster
//! (KUBECONFIG pointing at it). It self-skips otherwise, so a machine without a
//! cluster still passes `cargo test`. The cluster + the `awaken-bb:1` fixture image
//! are set up by `scripts/e2e/k8s_container_e2e.sh`, which runs this test.
#![cfg(feature = "k8s")]

use std::sync::Arc;
use std::time::Duration;

use awaken_provisioning_contract as pc;
use awaken_sandbox_container::ContainerProvider;
use awaken_sandbox_container::k8s::K8sRuntime;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The busybox `nc` fixture: listen on 8080 and, per connection, read the prompt and
/// reply with the newline-wire message + turn_end (the same stand-in the Docker e2e
/// and the namespace-tier ACP e2e use).
fn agent_argv() -> Vec<String> {
    let script = "read _p; \
        printf '%s\\n' '{\"type\":\"message\",\"text\":\"sandboxed reply\"}'; \
        printf '%s\\n' '{\"type\":\"turn_end\",\"reason\":\"natural_end\"}'";
    vec![
        "nc".into(),
        "-lk".into(),
        "-p".into(),
        "8080".into(),
        "-e".into(),
        "sh".into(),
        "-c".into(),
        script.into(),
    ]
}

fn spec(scope: &str) -> pc::SandboxSpec {
    pc::SandboxSpec {
        scope: scope.into(),
        isolation: pc::IsolationClass::Container,
        mounts: Vec::new(),
        env: Vec::new(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: "/mnt/session/outputs".into(),
        limits: pc::ResourceLimits::default(),
        lease_ttl_secs: None,
        extra: Some(serde_json::json!({ "command": agent_argv(), "image": "awaken-bb:1" })),
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
    vec![
        "nc".into(),
        "-lk".into(),
        "-p".into(),
        "8080".into(),
        "-e".into(),
        "sh".into(),
        "-c".into(),
        script.into(),
    ]
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
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: "/mnt/session/outputs".into(),
        limits: pc::ResourceLimits::default(),
        lease_ttl_secs: None,
        extra: Some(serde_json::json!({ "command": cat_mount_argv(), "image": "awaken-bb:1" })),
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
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: "/mnt/session/outputs".into(),
        limits: pc::ResourceLimits::default(),
        lease_ttl_secs: None,
        extra: Some(serde_json::json!({ "command": cat_mount_argv(), "image": "awaken-bb:1" })),
    }
}

/// A busybox fixture that greps for an ASCII marker embedded in a BINARY file (the file
/// begins with non-UTF-8 bytes) — proving a `binaryData` ConfigMap reached the Pod intact.
fn grep_binary_argv() -> Vec<String> {
    let script = "read _p; \
        if grep -q binary-marker-ok /acp-config/config.toml; then M=binary-ok; else M=binary-missing; fi; \
        printf '{\"type\":\"message\",\"text\":\"%s\"}\\n' \"$M\"; \
        printf '%s\\n' '{\"type\":\"turn_end\",\"reason\":\"natural_end\"}'";
    vec![
        "nc".into(),
        "-lk".into(),
        "-p".into(),
        "8080".into(),
        "-e".into(),
        "sh".into(),
        "-c".into(),
        script.into(),
    ]
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
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: "/mnt/session/outputs".into(),
        limits: pc::ResourceLimits::default(),
        lease_ttl_secs: None,
        extra: Some(serde_json::json!({ "command": grep_binary_argv(), "image": "awaken-bb:1" })),
    }
}

fn kubectl(args: &[&str]) -> std::process::Output {
    std::process::Command::new("kubectl")
        .args(args)
        .output()
        .expect("kubectl runs")
}

#[tokio::test]
async fn a_pod_agent_speaks_the_wire_over_a_port_forward() {
    if std::env::var("AWAKEN_K8S_E2E").as_deref() != Ok("1") {
        eprintln!("skipping: set AWAKEN_K8S_E2E=1 with a reachable cluster to run");
        return;
    }
    // The cluster must be reachable (kubeconfig points at it).
    if !kubectl(&["get", "nodes"]).status.success() {
        eprintln!("skipping: no reachable Kubernetes cluster");
        return;
    }

    // A fixed local port the runtime direct-dials; `kubectl port-forward` bridges it to
    // the Pod's 8080 once the Pod is Running.
    let local_port: u16 = 18080;
    let addr = format!("127.0.0.1:{local_port}").parse().unwrap();
    let scope = format!("k8s-e2e-{}", std::process::id());
    let pod = format!("awaken-{scope}");
    // Reap any leftover Pod from an interrupted prior run.
    let _ = kubectl(&["delete", "pod", &pod, "--ignore-not-found", "--now"]);

    let runtime = K8sRuntime::connect("default", addr)
        .await
        .expect("connect to the cluster");
    let provider = ContainerProvider::new(Arc::new(runtime), "awaken-bb:1");

    let sandbox = provider
        .create_container(&spec(&scope))
        .await
        .expect("create the agent Pod");

    // Wait for the Pod to be Running before forwarding to it.
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

    // Forward the Pod's agent port to the fixed local port the runtime dials.
    let mut forward = std::process::Command::new("kubectl")
        .args([
            "port-forward",
            &format!("pod/{pod}"),
            &format!("{local_port}:8080"),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn port-forward");

    // Dial + exchange the wire (retry while the forward + nc come up).
    let mut got = String::new();
    if ready {
        let mut channel = None;
        for _ in 0..100 {
            match awaken_agent_channel::AgentTransport::open_channel(&sandbox).await {
                Ok(c) => {
                    channel = Some(c);
                    break;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(200)).await,
            }
        }
        if let Some(mut channel) = channel {
            let _ = channel.write_all(b"hello\n").await;
            let _ = channel.flush().await;
            let mut buf = vec![0u8; 512];
            for _ in 0..50 {
                match tokio::time::timeout(Duration::from_secs(2), channel.read(&mut buf)).await {
                    Ok(Ok(0)) => break,
                    Ok(Ok(n)) => {
                        got.push_str(&String::from_utf8_lossy(&buf[..n]));
                        if got.contains("turn_end") {
                            break;
                        }
                    }
                    _ => break,
                }
            }
        }
    }

    // Clean up: stop the forward and reap the Pod before asserting.
    let _ = forward.kill();
    let _ = pc::Sandbox::dispose(&sandbox).await;
    let _ = kubectl(&["delete", "pod", &pod, "--ignore-not-found", "--now"]);

    assert!(ready, "the agent Pod must reach Running");
    assert!(
        got.contains("sandboxed reply"),
        "the Pod agent's reply must reach the host over the forwarded port: {got:?}"
    );
    assert!(got.contains("turn_end"), "the turn completed: {got:?}");
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

    let local_port: u16 = 18081;
    let addr = format!("127.0.0.1:{local_port}").parse().unwrap();
    let scope = format!("k8s-cfg-{}", std::process::id());
    let pod = format!("awaken-{scope}");
    let marker = "inline-configmap-marker-42";
    let _ = kubectl(&["delete", "pod", &pod, "--ignore-not-found", "--now"]);

    let runtime = K8sRuntime::connect("default", addr)
        .await
        .expect("connect to the cluster");
    let provider = ContainerProvider::new(Arc::new(runtime), "awaken-bb:1");

    let sandbox = provider
        .create_container(&inline_spec(&scope, marker))
        .await
        .expect("create the agent Pod with a ConfigMap-backed inline mount");

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

    let mut forward = std::process::Command::new("kubectl")
        .args([
            "port-forward",
            &format!("pod/{pod}"),
            &format!("{local_port}:8080"),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn port-forward");

    let mut got = String::new();
    if ready {
        let mut channel = None;
        for _ in 0..100 {
            match awaken_agent_channel::AgentTransport::open_channel(&sandbox).await {
                Ok(c) => {
                    channel = Some(c);
                    break;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(200)).await,
            }
        }
        if let Some(mut channel) = channel {
            let _ = channel.write_all(b"hello\n").await;
            let _ = channel.flush().await;
            let mut buf = vec![0u8; 512];
            for _ in 0..50 {
                match tokio::time::timeout(Duration::from_secs(2), channel.read(&mut buf)).await {
                    Ok(Ok(0)) => break,
                    Ok(Ok(n)) => {
                        got.push_str(&String::from_utf8_lossy(&buf[..n]));
                        if got.contains("turn_end") {
                            break;
                        }
                    }
                    _ => break,
                }
            }
        }
    }

    let _ = forward.kill();
    let _ = pc::Sandbox::dispose(&sandbox).await;
    let _ = kubectl(&["delete", "pod", &pod, "--ignore-not-found", "--now"]);

    // The ConfigMap held the inline bytes verbatim.
    assert_eq!(
        cm_data, marker,
        "the ConfigMap must hold the inline content"
    );
    assert!(ready, "the agent Pod must reach Running");
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

    let local_port: u16 = 18082;
    let addr = format!("127.0.0.1:{local_port}").parse().unwrap();
    let scope = format!("k8s-file-{}", std::process::id());
    let pod = format!("awaken-{scope}");
    let marker = "file-via-blobsource-99";
    let _ = kubectl(&["delete", "pod", &pod, "--ignore-not-found", "--now"]);

    let runtime = K8sRuntime::connect("default", addr)
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

    let mut forward = std::process::Command::new("kubectl")
        .args([
            "port-forward",
            &format!("pod/{pod}"),
            &format!("{local_port}:8080"),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn port-forward");

    let mut got = String::new();
    if ready {
        let mut channel = None;
        for _ in 0..100 {
            match awaken_agent_channel::AgentTransport::open_channel(&sandbox).await {
                Ok(c) => {
                    channel = Some(c);
                    break;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(200)).await,
            }
        }
        if let Some(mut channel) = channel {
            let _ = channel.write_all(b"hello\n").await;
            let _ = channel.flush().await;
            let mut buf = vec![0u8; 512];
            for _ in 0..50 {
                match tokio::time::timeout(Duration::from_secs(2), channel.read(&mut buf)).await {
                    Ok(Ok(0)) => break,
                    Ok(Ok(n)) => {
                        got.push_str(&String::from_utf8_lossy(&buf[..n]));
                        if got.contains("turn_end") {
                            break;
                        }
                    }
                    _ => break,
                }
            }
        }
    }

    let _ = forward.kill();
    let _ = pc::Sandbox::dispose(&sandbox).await;
    let _ = kubectl(&["delete", "pod", &pod, "--ignore-not-found", "--now"]);

    assert!(ready, "the agent Pod must reach Running");
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

    let local_port: u16 = 18084;
    let addr = format!("127.0.0.1:{local_port}").parse().unwrap();
    let scope = format!("k8s-bin-{}", std::process::id());
    let pod = format!("awaken-{scope}");
    let _ = kubectl(&["delete", "pod", &pod, "--ignore-not-found", "--now"]);

    let runtime = K8sRuntime::connect("default", addr)
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

    let mut forward = std::process::Command::new("kubectl")
        .args([
            "port-forward",
            &format!("pod/{pod}"),
            &format!("{local_port}:8080"),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn port-forward");

    let mut got = String::new();
    if ready {
        let mut channel = None;
        for _ in 0..100 {
            match awaken_agent_channel::AgentTransport::open_channel(&sandbox).await {
                Ok(c) => {
                    channel = Some(c);
                    break;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(200)).await,
            }
        }
        if let Some(mut channel) = channel {
            let _ = channel.write_all(b"hello\n").await;
            let _ = channel.flush().await;
            let mut buf = vec![0u8; 512];
            for _ in 0..50 {
                match tokio::time::timeout(Duration::from_secs(2), channel.read(&mut buf)).await {
                    Ok(Ok(0)) => break,
                    Ok(Ok(n)) => {
                        got.push_str(&String::from_utf8_lossy(&buf[..n]));
                        if got.contains("turn_end") {
                            break;
                        }
                    }
                    _ => break,
                }
            }
        }
    }

    let _ = forward.kill();
    let _ = pc::Sandbox::dispose(&sandbox).await;
    let _ = kubectl(&["delete", "pod", &pod, "--ignore-not-found", "--now"]);

    assert!(ready, "the agent Pod must reach Running");
    // The binary file (non-UTF-8 prefix + ASCII marker) reached the Pod intact via binaryData.
    assert!(
        got.contains("binary-ok"),
        "a binary File must reach the Pod via a binaryData ConfigMap: {got:?}"
    );
}
