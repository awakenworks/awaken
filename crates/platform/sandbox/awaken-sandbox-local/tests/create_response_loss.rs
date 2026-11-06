//! T27-C03 (create/adopt 证据), native Local provider: the creating process
//! dies after the physical create completed but before its response reached
//! anyone, while the realized Sandbox is still on disk. A new provider object
//! then recovers with the ORIGINAL preparation identity (same spec, same
//! effect fence).
//!
//! Oracles (groups-4.json order):
//! - O0 the recovery observes the same candidate physical instance (the bytes
//!   the dead creator wrote are still there, under the same root);
//! - O1 the successful adoption's handle agrees with the replayed create
//!   (one durable realization, one identity);
//! - O2 the retry creates no second Environment (the provider base holds the
//!   same entries before and after; a different operation is refused).
//!
//! | Row | Cause | Effect |
//! |---|---|---|
//! | L1 | child creates under fence F, writes a sentinel, prints, is killed | the Sandbox exists, no handle was delivered |
//! | R1 | new provider retries create with (spec, F) | Ready; same handle id; sentinel intact; no new entry in the base |
//! | R2 | adopt the replayed handle with F | same handle; sentinel intact |
//! | R3 | retry with another operation G on the same scope | refused; base unchanged; sentinel intact |
//! | NC | a fresh wrapper: same spec created on an empty base | Ready, but the O0 checker rejects it (no sentinel, other root) |
//!
//! Negative control: replacing the original instance with a new wrapper and
//! calling that recovery must fail. It is executed: the same O0 checker that
//! accepts R1 rejects the fresh wrapper.
use awaken_provisioning_contract as pc;
use awaken_sandbox_local::LocalProvider;
use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Command, Stdio};
mod common;
use common::sh_command as sh;

const CHILD_ENV: &str = "AWAKEN_T27_C03_CHILD_BASE";
const CHILD_MARKER: &str = "AWAKEN_T27_C03_CHILD_MARKER";
const SCOPE: &str = "t27-c03-response-loss";
const SENTINEL: &[u8] = b"written-by-the-dead-creator";

fn spec() -> pc::SandboxSpec {
    pc::SandboxSpec {
        process_tree_supervision: false,
        scope: SCOPE.into(),
        isolation: pc::IsolationClass::Workdir,
        mounts: Vec::new(),
        env: Vec::new(),
        packages: Default::default(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: "/mnt/session/outputs".into(),
        requests: Default::default(),
        limits: Default::default(),
        filesystem_continuity: pc::FilesystemContinuity::Retained,
        lease_ttl_secs: None,
        control_services: Default::default(),
        environment: None,
        command: Vec::new(),
        deny_tool_egress: false,
    }
}

fn fence(operation_id: &str) -> pc::SandboxEffectFence {
    pc::SandboxEffectFence::new(operation_id, "t27-owner", "t27-runtime", 1, u64::MAX).unwrap()
}

async fn create(
    base: &Path,
    operation: &pc::SandboxEffectFence,
) -> Result<std::sync::Arc<dyn pc::Sandbox>, pc::SandboxError> {
    let provider: Box<dyn pc::SandboxProvider<Environment = dyn pc::Sandbox>> =
        Box::new(LocalProvider::new(base));
    provider
        .claim_for_effect(
            &spec(),
            pc::SandboxEffect::Fenced {
                fence: operation,
                authority: &pc::test_support::EffectAuthority::new(operation),
            },
            pc::SandboxRealizationIntent::Create,
        )
        .await
}

/// Every entry the provider owns under its base, recursively by name: a
/// second Environment would add one.
fn entries(base: &Path) -> BTreeSet<String> {
    fn walk(root: &Path, dir: &Path, depth: usize, out: &mut BTreeSet<String>) {
        let Ok(read) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in read.flatten() {
            let path = entry.path();
            out.insert(
                path.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            );
            if depth < 2 && path.is_dir() {
                walk(root, &path, depth + 1, out);
            }
        }
    }
    let mut out = BTreeSet::new();
    walk(base, base, 0, &mut out);
    out
}

async fn sentinel(sandbox: &dyn pc::Sandbox) -> Option<Vec<u8>> {
    let artifacts = sandbox.artifacts().await.ok()?;
    let note = artifacts
        .iter()
        .find(|a| a.path.ends_with("/sentinel.txt"))?;
    sandbox.read_artifact(&note.id).await.ok()
}

/// What a recovery observed of the instance it holds.
struct Recovered {
    id: String,
    ready: bool,
    sentinel: Option<Vec<u8>>,
}

async fn observe(sandbox: &dyn pc::Sandbox) -> Recovered {
    Recovered {
        id: sandbox.id().to_owned(),
        ready: matches!(sandbox.status().await, Ok(pc::SandboxStatus::Ready)),
        sentinel: sentinel(sandbox).await,
    }
}

/// O0: the recovery holds the candidate the dead creator realized.
fn same_instance(observed: &Recovered) -> bool {
    observed.ready && observed.id == SCOPE && observed.sentinel.as_deref() == Some(SENTINEL)
}

/// L1: the owned child. Ignored so it never runs on its own.
#[tokio::test]
#[ignore = "owned child invoked and killed by t27_c03_create_response_loss_recovers_the_original_instance"]
async fn t27_c03_creating_child() {
    let Some(base) = std::env::var_os(CHILD_ENV) else {
        return;
    };
    let sandbox = create(Path::new(&base), &fence("t27-create"))
        .await
        .unwrap();
    let process = sandbox
        .spawn(sh(format!(
            "printf '{}' > $AWAKEN_OUTPUTS_DIR/sentinel.txt",
            String::from_utf8_lossy(SENTINEL)
        )))
        .await
        .unwrap();
    assert_eq!(process.wait().await.unwrap().code, Some(0));
    std::fs::write(std::env::var(CHILD_MARKER).unwrap(), sandbox.id()).unwrap();
    // The response never leaves this process: the parent kills it here.
    loop {
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
}

fn kill_creator_after_create(base: &Path, marker: &Path) {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "t27_c03_creating_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, base)
        .env(CHILD_MARKER, marker)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !marker.is_file() {
        assert!(
            child.try_wait().unwrap().is_none(),
            "L1 the child exited before the post-create boundary"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "L1 the child reached the post-create boundary"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    child.kill().unwrap();
    let status = child.wait().unwrap();
    assert!(!status.success(), "L1 the creator was killed, not finished");
    assert_eq!(
        std::fs::read_to_string(marker).unwrap(),
        SCOPE,
        "L1 created"
    );
}

#[tokio::test]
async fn t27_c03_create_response_loss_recovers_the_original_instance() {
    let base = tempfile::tempdir().unwrap();
    let marker = tempfile::tempdir().unwrap();
    kill_creator_after_create(base.path(), &marker.path().join("created"));
    let before = entries(base.path());
    assert!(
        !before.is_empty(),
        "L1 the physical instance outlived its creator"
    );

    // R1: a new provider object replays the original preparation identity.
    let original = fence("t27-create");
    let replayed = create(base.path(), &original)
        .await
        .expect("R1 the original create identity is recoverable");
    let r1 = observe(replayed.as_ref()).await;
    assert!(
        same_instance(&r1),
        "R1/O0 the recovery holds the original instance"
    );
    assert_eq!(entries(base.path()), before, "R1/O2 no second Environment");

    // R2: adoption from the replayed handle agrees with it.
    let handle = replayed.handle();
    drop(replayed);
    let provider: Box<dyn pc::SandboxProvider<Environment = dyn pc::Sandbox>> =
        Box::new(LocalProvider::new(base.path()));
    let adopted = provider
        .adopt_for_effect(
            pc::SandboxAdoption::new(&spec(), &handle),
            pc::SandboxEffect::Fenced {
                fence: &original,
                authority: &pc::test_support::EffectAuthority::new(&original),
            },
        )
        .await
        .expect("R2 the replayed handle is adoptable");
    assert_eq!(
        adopted.handle(),
        handle,
        "R2/O1 adoption and replay share one identity"
    );
    assert!(same_instance(&observe(adopted.as_ref()).await), "R2/O0");
    drop(adopted);
    assert_eq!(entries(base.path()), before, "R2/O2 no second Environment");

    // R3: another operation on the same scope may not mint a second instance.
    let other = create(base.path(), &fence("t27-other-create")).await;
    assert!(
        other.is_err(),
        "R3/O2 a different create operation is refused"
    );
    assert_eq!(
        entries(base.path()),
        before,
        "R3/O2 refusal leaves the base unchanged"
    );
    let again = create(base.path(), &original).await.unwrap();
    assert!(
        same_instance(&observe(again.as_ref()).await),
        "R3/O0 the original survives"
    );

    // NC: a new wrapper over an empty base is a live Sandbox with the same
    // logical id, yet it is not the original instance; O0 must reject it.
    let elsewhere = tempfile::tempdir().unwrap();
    let wrapper = create(elsewhere.path(), &original).await.unwrap();
    let nc = observe(wrapper.as_ref()).await;
    assert!(
        nc.ready && nc.id == SCOPE,
        "NC the wrapper is a live same-id Sandbox"
    );
    assert!(
        !same_instance(&nc),
        "NC the O0 checker rejects a replacement instance"
    );
}
