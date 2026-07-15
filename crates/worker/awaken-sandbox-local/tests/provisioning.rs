//! End-to-end provisioning tests (ADR-0035, resource surface) over the pc
//! `SandboxProvider`: build a [`pc::SandboxSpec`] with typed [`pc::MountRequirement`]s,
//! let [`LocalProvider::create_sandbox`] realize a [`LocalSandbox`], and assert the
//! realized surface — content-addressed resolution, fail-closed integrity, and G3
//! path-jailing — behaves as designed. Skills are not a sandbox mount (ADR-0036).

use std::path::PathBuf;

use awaken_provisioning_contract as pc;
use awaken_provisioning_contract::Sandbox as _;
use awaken_sandbox_local::{LocalProvider, content_fingerprint};

fn unique_base(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("awaken-sbx-e2e-{}-{tag}", std::process::id()))
}

fn spec(scope: &str) -> pc::SandboxSpec {
    pc::SandboxSpec {
        scope: scope.into(),
        isolation: pc::IsolationClass::Workdir,
        mounts: Vec::new(),
        env: Vec::new(),
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: "/outputs".into(),
        limits: pc::ResourceLimits::default(),
        lease_ttl_secs: None,
        extra: None,
    }
}

fn resource_mount(
    id: &str,
    mount_path: &str,
    content_hash: Option<String>,
) -> pc::MountRequirement {
    pc::MountRequirement {
        mount_id: id.into(),
        source: pc::MountSource::Resource {
            resource_id: id.into(),
            content_hash,
        },
        mount_path: mount_path.into(),
        access: pc::MountAccess::ReadWrite,
        lifetime: pc::MountLifetime::PerRun,
        required: true,
    }
}

#[tokio::test]
async fn provision_resource_mount_end_to_end() {
    let base = unique_base("ok");
    let res_content = "alpha=1\nbeta=2\n";
    // The content is resolved by id from the seeded content-addressed store.
    let provider = LocalProvider::new(&base).with_blob("cfg", res_content.as_bytes().to_vec());
    let mut s = spec("e2e");
    s.mounts.push(resource_mount(
        "cfg",
        ".mnt/config/app.env",
        Some(content_fingerprint(res_content.as_bytes())),
    ));

    let env = provider
        .create_sandbox(&s)
        .await
        .expect("provision succeeds");

    // Realized under the `.mnt/` root, exposed by logical mount_path only (G3).
    let realized = env.realized();
    assert_eq!(realized.len(), 1);
    assert_eq!(realized[0].mount_path, ".mnt/config/app.env");
    assert!(!realized[0].mount_path.starts_with('/'));
    let on_disk = base.join("e2e").join(".mnt/config/app.env");
    assert_eq!(std::fs::read_to_string(&on_disk).unwrap(), res_content);

    env.dispose().await.unwrap();
}

#[tokio::test]
async fn provision_fails_closed_on_content_hash_mismatch() {
    let base = unique_base("bad-hash");
    let provider = LocalProvider::new(&base).with_blob("x", b"the real content".to_vec());
    let mut s = spec("bad");
    // A declared hash that does not match the seeded content.
    s.mounts
        .push(resource_mount("x", ".mnt/x.env", Some("deadbeef".into())));

    let err = provider
        .create_sandbox(&s)
        .await
        .err()
        .expect("must fail closed");
    assert!(
        err.to_string().contains("content hash mismatch"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn resource_mount_path_escape_fails_closed() {
    let base = unique_base("escape");
    let provider = LocalProvider::new(&base).with_blob("r", b"x".to_vec());
    let mut s = spec("esc");
    // A mount_path that tries to climb out of the jail.
    s.mounts.push(resource_mount("r", "../../etc/passwd", None));

    let err = provider
        .create_sandbox(&s)
        .await
        .err()
        .expect("escape must fail closed");
    assert!(err.to_string().contains("escape"), "unexpected: {err}");
}

#[tokio::test]
async fn a_required_mount_with_no_resolvable_source_fails_closed() {
    let base = unique_base("missing");
    // No seed for "gone": a required mount with nothing to resolve must fail.
    let provider = LocalProvider::new(&base);
    let mut s = spec("miss");
    s.mounts.push(resource_mount("gone", ".mnt/g", None));

    assert!(provider.create_sandbox(&s).await.is_err());
}

#[tokio::test]
async fn empty_spec_has_no_realized_mounts() {
    let base = unique_base("empty");
    let provider = LocalProvider::new(&base);
    let env = provider.create_sandbox(&spec("plain")).await.unwrap();
    assert!(env.realized().is_empty());
    // The full built-in capability surface is still composed.
    assert!(!env.rooted_tools().is_empty());
    env.dispose().await.unwrap();
}
