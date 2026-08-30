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
        packages: Default::default(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: "/mnt/session/outputs".into(),
        requests: Default::default(),
        limits: Default::default(),
        filesystem_continuity: awaken_provisioning_contract::FilesystemContinuity::Retained,
        lease_ttl_secs: None,
        control_services: Default::default(),
        environment: None,
        command: Vec::new(),
        deny_tool_egress: false,
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

fn fence(operation_id: &str) -> pc::SandboxEffectFence {
    pc::SandboxEffectFence::new(
        operation_id,
        "session-recovery-owner",
        "session-recovery-runtime",
        1,
        u64::MAX,
    )
    .unwrap()
}

fn authorization(prepared: &pc::SandboxEffectFence) -> pc::SandboxDisposalAuthorization {
    let fingerprint = format!("session-recovery-preparation:{}", prepared.operation_id);
    let preparation = pc::SandboxDisposalPreparation::new(prepared.clone(), fingerprint).unwrap();
    let operation_id = preparation.operation_id().unwrap();
    let successor = pc::SandboxEffectFence::new(
        operation_id,
        prepared.owner.clone(),
        prepared.runtime_incarnation.clone(),
        prepared.epoch,
        prepared.expires_at_unix_ms,
    )
    .unwrap();
    preparation.authorize(successor).unwrap()
}

/// Host A realizes + (if it can exec) writes an artifact; host B (a fresh provider over
/// the same durable root) adopts the serialized handle and recovers the sandbox.
async fn cross_node_adopt(tier: Tier, can_exec: bool) {
    // Cause/effect decision table: C1 Host A creates under one live aggregate
    // fence; C2 its V2 handle is serialized; C3 Host B presents the exact same
    // spec, handle, and realization fence; C4 a distinct terminal operation is
    // authorized by the same lease generation. R1 C1+C2+C3 => Host B adopts
    // the exact Ready incarnation and reads Host A's durable bytes. R2
    // R1+C4 => fenced disposal publishes the one Removed tombstone and reaps
    // the exact root. Legacy/V1 adoption remains deliberately non-destructive
    // and is not a substitute recovery authority.
    let base = tempfile::tempdir().unwrap();
    let sandbox_spec = spec(tier, "t-xnode");
    let realization_fence = fence("cross-node-create");

    // ── Host A: realize the sandbox, optionally write a durable artifact, persist. ──
    let host_a = provider(tier, base.path());
    let sandbox_a = host_a
        .create_for_effect(&sandbox_spec, &realization_fence)
        .await
        .unwrap();
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
        .adopt_for_effect(&sandbox_spec, &recovered, &realization_fence)
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

    // Cross-node disposal rule: the aggregate-authorized preparation call
    // binds the exact recovered filesystem participant without deleting it;
    // only the subsequent physical-disposal call may remove that participant.
    let terminal = fence("cross-node-terminal");
    sandbox_b
        .prepare_disposal_for_effect(&terminal)
        .await
        .unwrap();
    sandbox_b
        .dispose_for_effect(&authorization(&terminal))
        .await
        .unwrap();
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
