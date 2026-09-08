//! Strict Linux composition of the existing scoped owner and real Memory adapter.
//! No fake guard, successful substrate skip, or Copy fallback in this case.
use super::*;
use awaken_memory_store::{MemoryRepository, SqliteMemoryRepository};
use awaken_sandbox_local::LocalSandbox;
use awaken_sandbox_memoryd::MemoryStoreMounter;
use std::path::PathBuf;

const STORE: &str = "owner-crash-memory";
const BEFORE: &str = "committed before owner SIGKILL — 无换行";
const AFTER: &str = "committed by the recovered execution — 无换行";

fn repository(base: &Path) -> Arc<SqliteMemoryRepository> {
    Arc::new(SqliteMemoryRepository::open(base.join("memory.db").to_str().unwrap()).unwrap())
}

pub(super) fn provider(base: &Path) -> LocalProvider {
    LocalProvider::trusted_host(base)
        .with_memory_mounter(Arc::new(MemoryStoreMounter::new(repository(base))))
}

pub(super) fn specification() -> pc::SandboxSpec {
    let mut specification = spec();
    specification.mounts.push(pc::MountRequirement {
        mount_id: "memory".into(),
        mount_path: "/mnt/memory".into(),
        source: pc::MountSource::MemoryStore {
            store_id: STORE.into(),
            materialization_reference: None,
            write_consistency: pc::MemoryWriteConsistency::WriteThroughRequired,
        },
        access: pc::MountAccess::ReadWrite,
        lifetime: pc::MountLifetime::Durable,
        required: true,
    });
    specification
}

async fn edit(sandbox: &LocalSandbox, before: &str, after: &str) {
    // Physical provider path, passed as argv data (never shell interpolation).
    // The actual scoped child must read the old head, preserve byte exactness,
    // and fsync before the parent is permitted to inject the crash.
    let note = sandbox.workspace_path().join("mnt/memory/note.md");
    let cwd = sandbox.workspace_path().join("workspace/nested 工作目录");
    let mut command = pc::Command::new([
        "/usr/bin/python3",
        "-c",
        "import os,sys\nassert os.getcwd()==sys.argv[4]\np=sys.argv[1]\nassert open(p,'rb').read()==sys.argv[2].encode()\nwith open(p,'wb') as f:\n f.write(sys.argv[3].encode()); f.flush(); os.fsync(f.fileno())",
        note.to_str().unwrap(),
        before,
        after,
        cwd.to_str().unwrap(),
    ]);
    command.cwd = "/workspace/nested 工作目录".into();
    let process = sandbox.spawn(command).await.unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), process.wait())
            .await
            .unwrap()
            .unwrap()
            .code,
        Some(0)
    );
}

pub(super) async fn prepare_owner(base: &Path, sandbox: &LocalSandbox) {
    assert!(
        sandbox.realized().iter().any(|mount| {
            mount.mount_id == "memory" && mount.realization == pc::Realization::Fuse
        })
    );
    edit(sandbox, "seed", BEFORE).await;
    // A fixture handoff, not a new recovery receipt: adoption consumes the
    // provider's unmodified original handle and its existing durable marker.
    std::fs::write(
        base.join("owner-handle.json"),
        serde_json::to_vec(&(sandbox.handle(), sandbox.workspace_path())).unwrap(),
    )
    .unwrap();
}

fn kernel_mount(path: &Path) -> Option<String> {
    let escaped = path
        .to_str()
        .unwrap()
        .replace('\\', "\\134")
        .replace(' ', "\\040");
    std::fs::read_to_string("/proc/self/mountinfo")
        .unwrap()
        .lines()
        .find(|line| line.split_whitespace().nth(4) == Some(escaped.as_str()))
        .map(str::to_owned)
}

async fn durable_head(base: &Path) -> awaken_memory_store::Memory {
    // Independent reopened SQLite connection, not the FUSE content cache.
    repository(base)
        .get_by_path(STORE, "/note.md")
        .await
        .unwrap()
        .unwrap()
}

fn assert_disconnected(file: &mut std::fs::File) {
    let error = std::io::Read::read(file, &mut [0]).unwrap_err();
    // A request on the old FUSE fd returns Linux EIO (5); a newly resolved
    // path through the disconnected mount reports ENOTCONN instead.
    assert!(
        error.raw_os_error() == Some(5) || error.kind() == std::io::ErrorKind::NotConnected,
        "old FUSE connection must fail, not an unrelated I/O error: {error}"
    );
}

#[tokio::test]
#[ignore = "strict /dev/fuse + bwrap qualification; owned by sandbox_capability_suite G7"]
async fn provider_owner_crash_reacquires_fuse_before_new_scoped_execution() {
    // Cause/effect graph: C1 exact Ready/handle, C2 old tree alive, C3 owner
    // SIGKILL (no Drop), C4 identity known, C5 independent fsync durable head,
    // C6 fresh mounter, C7 terminal authorization, C8 old open FUSE fd. Rules:
    // F1 C1+C2+C5+C6 => reject adoption; same mount and DB head, no detach.
    // F2 C1+C3+!C4+C5+C6 => retain disconnected mount/root; reject adoption.
    // F3 C1+C3+C4+C5+C6+C8 => lazy-detach busy dead FUSE; old fd still fails,
    //    reacquired path works, and the new scoped child reads
    //    BEFORE then commits AFTER, same physical Session root and File bytes.
    // F4 F3+C7 => unmount before root removal; external DB/sentinel retained.
    // Constraint: all three detached tree sockets AND every recorded execution
    // must close before F2/F3. This proves Provider process death, not a full
    // distributed Worker/Control restart, vendor login or partial-I/O durability.
    let base = tempfile::tempdir().unwrap();
    repository(base.path())
        .create(STORE, "/note.md", "seed")
        .await
        .unwrap();
    std::fs::write(base.path().join("outside"), b"outside owned root").unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut owner = spawn_owner(base.path(), &listener, true);
    let nodes = observe_tree(&listener).await;
    let original = marker(base.path());
    // The writer's wait already proved exit; record_execution reclaims that
    // positively absent identity before recording the still-live tree.
    assert_eq!(original["executions"].as_array().unwrap().len(), 1);
    let (handle, root): (pc::SandboxHandle, PathBuf) =
        serde_json::from_slice(&std::fs::read(base.path().join("owner-handle.json")).unwrap())
            .unwrap();
    let mount_path = root.join("mnt/memory");
    let mounted = kernel_mount(&mount_path).expect("actual host FUSE mount");
    let mut old_file = std::fs::File::open(mount_path.join("note.md")).unwrap();
    let before = durable_head(base.path()).await;
    assert_eq!(before.content.as_deref(), Some(BEFORE));
    let provider = provider(base.path());
    let specification = specification();
    let authority = pc::test_support::EffectAuthority::new(&fence("adopt"));
    assert!(
        provider
            .adopt_sandbox_for_effect(&specification, &handle, &fence("adopt"), &authority)
            .await
            .is_err()
    );
    assert_tree_live(&nodes);
    assert_eq!(kernel_mount(&mount_path).as_ref(), Some(&mounted));
    assert_eq!(durable_head(base.path()).await, before);

    kill_owner_and_observe_exit(&mut owner, nodes, &original).await;
    assert_eq!(kernel_mount(&mount_path).as_ref(), Some(&mounted));
    assert_disconnected(&mut old_file);
    assert_eq!(
        std::fs::read(mount_path.join("note.md"))
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::NotConnected
    );
    let marker_path = base.path().join(".scoped-provider.awaken-realization.json");
    let mut foreign = original.clone();
    for execution in foreign["executions"].as_array_mut().unwrap() {
        execution["boot_id"][0] =
            serde_json::json!((execution["boot_id"][0].as_u64().unwrap() + 1) % 256);
    }
    std::fs::write(&marker_path, serde_json::to_vec(&foreign).unwrap()).unwrap();
    assert!(
        provider
            .adopt_sandbox_for_effect(&specification, &handle, &fence("adopt"), &authority)
            .await
            .is_err()
    );
    assert_eq!(kernel_mount(&mount_path).as_ref(), Some(&mounted));
    assert_eq!(durable_head(base.path()).await, before);
    assert_eq!(
        std::fs::read(root.join("mnt/session/file")).unwrap(),
        b"exact\0file\xff"
    );

    std::fs::write(&marker_path, serde_json::to_vec(&original).unwrap()).unwrap();
    let adopted = provider
        .adopt_sandbox_for_effect(&specification, &handle, &fence("adopt"), &authority)
        .await
        .unwrap();
    assert_eq!(adopted.workspace_path(), root);
    assert!(kernel_mount(&mount_path).is_some(), "reacquired real FUSE");
    // Linux can immediately recycle mount IDs after unmount. An old open file
    // must remain disconnected while the newly resolved path works; keeping it
    // open also covers the existing lazy-detach fallback for a busy dead mount.
    assert_disconnected(&mut old_file);
    assert_eq!(
        std::fs::read(mount_path.join("note.md")).unwrap(),
        BEFORE.as_bytes()
    );
    drop(old_file);
    assert_eq!(
        durable_head(base.path()).await,
        before,
        "remount does not write back stale bytes"
    );
    adopted
        .install_scoped_execution(
            Path::new("/usr/bin/bwrap"),
            Path::new(sandbox_binary()),
            Authority::live(),
        )
        .unwrap();
    edit(&adopted, BEFORE, AFTER).await;
    let after = durable_head(base.path()).await;
    assert_eq!(after.content.as_deref(), Some(AFTER));
    assert!(after.version > before.version);
    assert_eq!(
        std::fs::read(root.join("mnt/session/file")).unwrap(),
        b"exact\0file\xff"
    );
    dispose(&adopted).await;
    assert!(kernel_mount(&mount_path).is_none());
    assert_eq!(marker(base.path())["phase"], "removed");
    assert_eq!(durable_head(base.path()).await, after);
    assert_eq!(
        std::fs::read(base.path().join("outside")).unwrap(),
        b"outside owned root"
    );
}
