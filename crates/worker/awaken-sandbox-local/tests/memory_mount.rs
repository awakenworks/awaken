//! Memory-store realization end-to-end (ADR-0053 item 1): the LocalProvider realizes
//! a `MountSource::MemoryStore` through the injected worker-tier `MemoryStoreMounter`
//! (FUSE where `/dev/fuse` is present, else a harvested copy), and an edit in the
//! sandbox propagates back to the durable store on dispose. Without a mounter, a
//! memory mount fails loud rather than being faked.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use awaken_memory_store::{MemoryRepository, VolatileMemoryRepository};
use awaken_provisioning_contract as pc;
use awaken_provisioning_contract::SandboxProvider;
use awaken_sandbox_local::LocalProvider;
use awaken_sandbox_memoryd::MemoryStoreMounter;

fn base_spec(scope: &str) -> pc::SandboxSpec {
    pc::SandboxSpec {
        scope: scope.into(),
        isolation: pc::IsolationClass::Workdir,
        mounts: Vec::new(),
        env: Vec::new(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: "/mnt/session/outputs".into(),
        limits: Default::default(),
        lease_ttl_secs: None,
        extra: None,
    }
}

fn memory_mount(store_id: &str, access: pc::MountAccess) -> pc::MountRequirement {
    pc::MountRequirement {
        mount_id: "mem".into(),
        source: pc::MountSource::MemoryStore {
            store_id: store_id.into(),
        },
        mount_path: "/mnt/memory".into(),
        access,
        lifetime: pc::MountLifetime::Durable,
        required: true,
    }
}

/// Find the first file named `name` anywhere under `root` (walks into the FUSE mount).
fn find_file(root: &Path, name: &str) -> Option<PathBuf> {
    for entry in std::fs::read_dir(root).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(hit) = find_file(&path, name) {
                return Some(hit);
            }
        } else if path.file_name().and_then(|n| n.to_str()) == Some(name) {
            return Some(path);
        }
    }
    None
}

#[tokio::test]
async fn memory_store_mount_realizes_and_persists_an_edit() {
    let fs = Arc::new(VolatileMemoryRepository::new());
    fs.create("s", "/note.md", "v1").await.unwrap();
    let mounter = Arc::new(MemoryStoreMounter::new(fs.clone()));

    let tmp = tempfile::tempdir().unwrap();
    let provider = LocalProvider::new(tmp.path()).with_memory_mounter(mounter);

    let mut spec = base_spec("t-mem");
    spec.mounts
        .push(memory_mount("s", pc::MountAccess::ReadWrite));

    let sandbox = provider.create(&spec).await.unwrap();
    let realization = sandbox.realized()[0].realization;
    assert!(
        matches!(realization, pc::Realization::Fuse | pc::Realization::Copy),
        "a memory store realizes as FUSE (live) or Copy (fallback)"
    );

    // The store's seeded memory is visible in the sandbox (through the mount).
    let note = find_file(tmp.path(), "note.md").expect("seeded memory is realized");
    assert_eq!(std::fs::read_to_string(&note).unwrap(), "v1");

    // An agent edits the file; dispose unmounts (FUSE write-through) / harvests (copy).
    std::fs::write(&note, "v2").unwrap();
    sandbox.dispose().await.unwrap();

    assert_eq!(
        fs.get_by_path("s", "/note.md")
            .await
            .unwrap()
            .unwrap()
            .content
            .as_deref(),
        Some("v2"),
        "the edit propagated back to the durable store (realized as {realization:?})"
    );
}

#[tokio::test]
async fn memory_store_mount_fails_loud_without_a_mounter() {
    let tmp = tempfile::tempdir().unwrap();
    let provider = LocalProvider::new(tmp.path()); // no mounter wired

    let mut spec = base_spec("t-nomem");
    spec.mounts
        .push(memory_mount("s", pc::MountAccess::ReadWrite));

    let err = match provider.create(&spec).await {
        Err(e) => e,
        Ok(_) => panic!("a memory mount with no mounter must fail, not succeed"),
    };
    assert!(
        err.to_string().contains("no memory mounter wired"),
        "unexpected error: {err}"
    );
    // The whole environment is reaped on the failed mount (all-or-nothing).
    assert!(
        find_file(tmp.path(), "note.md").is_none(),
        "no partial sandbox directory survives"
    );
}
