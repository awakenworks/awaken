//! End-to-end over the REAL `LocalProvider`: one sandbox with a memory-store mount, a
//! secret mount, and an outputs dir. A real child process edits memory and writes an
//! artifact; teardown then harvests the memory write-back, collects the artifact, and
//! shreds+reaps — proving the `dispose` ordering (harvest → shred → reap) end-to-end
//! (no mocks: real FS, real process, the real worker-tier memory mounter).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use awaken_memory_store::{InMemoryFs, MemoryFs};
use awaken_provisioning_contract::{self as pc, Sandbox};
use awaken_sandbox_local::LocalProvider;
use awaken_sandbox_memoryd::MemoryStoreMounter;

fn sh(script: &str) -> pc::Command {
    let mut c = pc::Command::new(["sh", "-c", script]);
    c.stdio = pc::Stdio::Null;
    c
}

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
async fn full_lifecycle_harvests_memory_collects_outputs_then_shreds_and_reaps() {
    let fs = Arc::new(InMemoryFs::new());
    fs.create("mem", "/notes.md", "seed").await.unwrap();
    let mounter = Arc::new(MemoryStoreMounter::new(fs.clone()));

    let tmp = tempfile::tempdir().unwrap();
    let provider = LocalProvider::new(tmp.path())
        .with_memory_mounter(mounter)
        .with_blob("broker://key", b"sk-secret".to_vec());

    let spec = pc::SandboxSpec {
        scope: "t-life".into(),
        isolation: pc::IsolationClass::Workdir,
        mounts: vec![
            pc::MountRequirement {
                mount_id: "mem".into(),
                source: pc::MountSource::MemoryStore {
                    store_id: "mem".into(),
                },
                mount_path: "/mnt/memory".into(),
                access: pc::MountAccess::ReadWrite,
                lifetime: pc::MountLifetime::Durable,
                required: true,
            },
            pc::MountRequirement {
                mount_id: "auth".into(),
                source: pc::MountSource::Secret {
                    reference: "broker://key".into(),
                    content_hash: None,
                },
                mount_path: "/workspace/.auth".into(),
                access: pc::MountAccess::ReadWrite,
                lifetime: pc::MountLifetime::PerRun,
                required: true,
            },
        ],
        env: Vec::new(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: "/mnt/session/outputs".into(),
        limits: pc::ResourceLimits::default(),
        lease_ttl_secs: None,
        extra: None,
    };

    let sandbox = provider.create_sandbox(&spec).await.unwrap();

    // The seeded memory is realized into the sandbox; the agent edits it.
    let note = find_file(tmp.path(), "notes.md").expect("memory realized into the sandbox");
    assert_eq!(std::fs::read_to_string(&note).unwrap(), "seed");
    std::fs::write(&note, "edited").unwrap();

    // A real child writes an artifact to the outputs dir.
    let proc = sandbox
        .spawn(sh(
            r#"printf 'artifact-bytes' > "$AWAKEN_OUTPUTS_DIR/out.txt""#,
        ))
        .await
        .unwrap();
    assert_eq!(proc.wait().await.unwrap().code, Some(0));

    // Artifacts are collected out of the outputs dir (before teardown).
    let arts = sandbox.artifacts().await.unwrap();
    let out = arts
        .iter()
        .find(|a| a.path.ends_with("out.txt"))
        .expect("the written artifact is collected");
    assert_eq!(
        sandbox.read_artifact(&out.id).await.unwrap(),
        b"artifact-bytes"
    );

    // Teardown: harvest memory (persist the edit) → shred the secret → reap the dir.
    sandbox.dispose().await.unwrap();

    // The memory edit reached the durable store — harvest ran before the reap.
    assert_eq!(
        fs.get_by_path("mem", "/notes.md")
            .await
            .unwrap()
            .unwrap()
            .content
            .as_deref(),
        Some("edited"),
        "memory write-back was harvested before shred/reap"
    );
    // The whole environment is gone.
    assert!(matches!(
        sandbox.status().await.unwrap(),
        pc::SandboxStatus::Terminated
    ));
    assert!(find_file(tmp.path(), "out.txt").is_none(), "outputs reaped");
}
