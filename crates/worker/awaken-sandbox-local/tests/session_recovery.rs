//! Cross-node sandbox recovery: a sandbox realized by one host is adopted by a
//! **different host process** from its persisted [`pc::SandboxHandle`], and the durable
//! state (artifacts the first host's process wrote) survives the move.
//!
//! The existing `handle_serializes_and_adopt_reconnects` / `adopt_reconnects_from_a_
//! persisted_handle` tests adopt on the *same* provider instance (a restart of the same
//! host). This exercises the harder case: a **fresh provider object** — the stand-in for
//! a second worker/node — pointed at the same durable sandbox root, takes over. (For a
//! container tier the same shape is covered in `src/tests.rs` over a shared runtime.)
//!
//! Cross-*directory* / cross-*machine* ACP session recovery (harvest a CLI's session
//! subtree from config home A, restore into config home B via a shared blob store) is
//! covered deterministically in `awaken-run-executor-acp/src/session_home.rs`.

use awaken_provisioning_contract as pc;
use awaken_sandbox_local::{LocalProvider, NamespaceProvider};

#[derive(Clone, Copy, Debug)]
enum Tier {
    Workdir,
    Namespace,
}

async fn bwrap_works() -> bool {
    tokio::process::Command::new("bwrap")
        .args(["--unshare-user", "--ro-bind", "/", "/", "--", "true"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

fn provider(tier: Tier, base: &std::path::Path) -> Box<dyn pc::SandboxProvider> {
    match tier {
        Tier::Workdir => Box::new(LocalProvider::new(base)),
        Tier::Namespace => Box::new(NamespaceProvider::new(base)),
    }
}

fn spec(tier: Tier, scope: &str) -> pc::SandboxSpec {
    pc::SandboxSpec {
        scope: scope.into(),
        isolation: match tier {
            Tier::Workdir => pc::IsolationClass::Workdir,
            Tier::Namespace => pc::IsolationClass::Namespace,
        },
        mounts: Vec::new(),
        env: Vec::new(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: "/mnt/session/outputs".into(),
        limits: Default::default(),
        lease_ttl_secs: None,
        extra: None,
    }
}

fn out(tier: Tier, name: &str) -> String {
    match tier {
        Tier::Workdir => format!("$AWAKEN_OUTPUTS_DIR/{name}"),
        Tier::Namespace => format!("/mnt/session/outputs/{name}"),
    }
}

fn sh(script: String) -> pc::Command {
    let mut c = pc::Command::new(["sh", "-c", script.as_str()]);
    c.stdio = pc::Stdio::Null;
    c
}

/// Host A realizes + (if it can exec) writes an artifact; host B (a fresh provider over
/// the same durable root) adopts the serialized handle and recovers the sandbox.
async fn cross_node_adopt(tier: Tier, can_exec: bool) {
    let base = tempfile::tempdir().unwrap();

    // ── Host A: realize the sandbox, optionally write a durable artifact, persist. ──
    let host_a = provider(tier, base.path());
    let sandbox_a = host_a.create(&spec(tier, "t-xnode")).await.unwrap();
    assert_eq!(sandbox_a.id(), "t-xnode");

    if can_exec {
        let proc = sandbox_a
            .spawn(sh(format!(
                "printf 'from-host-a' > {}",
                out(tier, "note.txt")
            )))
            .await
            .unwrap();
        assert_eq!(proc.wait().await.unwrap().code, Some(0));
    }

    let wire = serde_json::to_string(&sandbox_a.handle()).unwrap();
    // Host A goes away entirely (drop the provider + the live sandbox).
    drop(sandbox_a);
    drop(host_a);

    // ── Host B: a brand-new provider object (a different worker/node) adopts it. ──
    let recovered: pc::SandboxHandle = serde_json::from_str(&wire).unwrap();
    let host_b = provider(tier, base.path());
    let sandbox_b = host_b
        .adopt(&recovered)
        .await
        .unwrap_or_else(|e| panic!("{tier:?}: a second host must adopt the handle: {e:?}"));

    assert_eq!(sandbox_b.id(), "t-xnode");
    assert!(
        matches!(sandbox_b.status().await.unwrap(), pc::SandboxStatus::Ready),
        "{tier:?}: the adopted sandbox is live on the new host"
    );
    // The new host can keep it alive (renew the dead-man's lease).
    sandbox_b.renew_lease().await.unwrap();

    // Durable state written by host A is visible to host B.
    if can_exec {
        let arts = sandbox_b.artifacts().await.unwrap();
        let note = arts
            .iter()
            .find(|a| a.path.ends_with("/note.txt"))
            .unwrap_or_else(|| panic!("{tier:?}: host A's artifact survived the node move"));
        assert_eq!(
            sandbox_b.read_artifact(&note.id).await.unwrap(),
            b"from-host-a",
            "{tier:?}: the adopting host reads the durable artifact"
        );
    }

    sandbox_b.dispose().await.unwrap();
    assert!(matches!(
        sandbox_b.status().await.unwrap(),
        pc::SandboxStatus::Terminated
    ));
}

#[tokio::test]
async fn workdir_sandbox_is_adopted_by_a_second_host() {
    cross_node_adopt(Tier::Workdir, true).await;
}

#[tokio::test]
async fn namespace_sandbox_is_adopted_by_a_second_host() {
    let bwrap = bwrap_works().await;
    if !bwrap {
        eprintln!(
            "note: bwrap/userns unavailable — the artifact-survival step self-skips; \
                   realize + cross-host adopt + status/lease still run"
        );
    }
    cross_node_adopt(Tier::Namespace, bwrap).await;
}
