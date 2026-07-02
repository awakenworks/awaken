//! End-to-end provisioning tests (ADR-0035, resource surface).
//!
//! Exercise the public seam only: build a [`SandboxSpec`] with typed resource
//! mounts, let `LocalSandboxProvider::create` provision an [`Environment`], and
//! assert the capability surface — realized resources, the host-side receipt,
//! forward-compat, and fail-closed integrity — behaves as designed. Skills are no
//! longer a sandbox mount (ADR-0036): they are fronted by the single `Skill` tool
//! in `awaken-ext-skills`, not surfaced as per-skill environment tools.

use std::path::PathBuf;

use awaken_sandbox_local::{
    LocalSandboxProvider, Mount, ProvisionKind, ResourceMount, SandboxProvider, SandboxSpec,
    content_fingerprint,
};

fn unique_base(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("awaken-sbx-e2e-{}-{tag}", std::process::id()))
}

#[tokio::test]
async fn provision_resource_end_to_end() {
    let base = unique_base("ok");
    let provider = LocalSandboxProvider::new(&base);

    let res_content = "alpha=1\nbeta=2\n";
    let mut spec = SandboxSpec::new("e2e").with_mount(Mount::Resource(ResourceMount {
        id: "cfg".into(),
        content_hash: content_fingerprint(res_content.as_bytes()),
        logical_path: "config/app.env".into(),
        content: res_content.into(),
    }));
    // Forward-compat: a mount shape this provider does not understand is ignored,
    // never an error (mounts is an opaque `Value` carrier).
    spec.mounts
        .push(serde_json::json!({ "kind": "future_thing", "whatever": 42 }));

    let env = provider.create(&spec).await.expect("provision succeeds");

    // Capability surface: the six isolation hand tools, nothing skill-shaped.
    let ids: Vec<String> = env.tools().iter().map(|t| t.id().to_string()).collect();
    for hand in ["read", "write", "edit", "glob", "grep", "bash"] {
        assert!(ids.contains(&hand.to_string()), "missing hand tool {hand}");
    }
    assert!(
        !ids.iter().any(|id| id.starts_with("skill")),
        "no skill-shaped tool may be on the capability surface: {ids:?}"
    );
    assert_eq!(env.hand_tools().len(), 6);

    // The resource is realized under the read-only `.mnt/` root and exposed by
    // logical path only — no host absolute path crosses the boundary (G3).
    let refs = env.resources();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].id, "cfg");
    assert_eq!(refs[0].logical_path, ".mnt/config/app.env");
    assert!(!refs[0].logical_path.starts_with('/'));
    let realized = base.join("e2e").join(".mnt/config/app.env");
    assert_eq!(std::fs::read_to_string(&realized).unwrap(), res_content);

    // The receipt is the host-side pin: the provisioned entry, content-addressed.
    let receipt = env.receipt();
    assert_eq!(receipt.entries.len(), 1);
    assert!(
        receipt.entries.iter().any(|e| e.id == "cfg"
            && e.kind == ProvisionKind::Resource
            && !e.content_hash.is_empty())
    );

    provider.teardown("e2e").await.unwrap();
}

#[tokio::test]
async fn reprovision_from_same_spec_yields_identical_receipt() {
    // Cold replay baseline: the same spec provisions an identical (content-addressed)
    // environment, so the receipt — the pin — is stable across provisions.
    let base = unique_base("replay");
    let provider = LocalSandboxProvider::new(&base);
    let content = "beta=2\n";
    let spec = SandboxSpec::new("r").with_mount(Mount::Resource(ResourceMount {
        id: "cfg".into(),
        content_hash: content_fingerprint(content.as_bytes()),
        logical_path: "config/app.env".into(),
        content: content.into(),
    }));

    let a = provider.create(&spec).await.unwrap();
    provider.teardown("r").await.unwrap();
    let b = provider.create(&spec).await.unwrap();

    assert_eq!(a.receipt(), b.receipt());
    provider.teardown("r").await.unwrap();
}

#[tokio::test]
async fn provision_fails_closed_on_content_hash_mismatch() {
    let base = unique_base("bad-hash");
    let provider = LocalSandboxProvider::new(&base);
    let spec = SandboxSpec::new("bad").with_mount(Mount::Resource(ResourceMount {
        id: "x".into(),
        content_hash: "deadbeef".into(), // does not match the content
        logical_path: "x.env".into(),
        content: "the real content".into(),
    }));

    let err = provider.create(&spec).await.expect_err("must fail closed");
    assert!(
        err.to_string().contains("content hash mismatch"),
        "unexpected error: {err}"
    );
    let _ = provider.teardown("bad").await;
}

#[tokio::test]
async fn resource_path_escape_fails_closed() {
    let base = unique_base("escape");
    let provider = LocalSandboxProvider::new(&base);
    let content = "x";
    let spec = SandboxSpec::new("esc").with_mount(Mount::Resource(ResourceMount {
        id: "r".into(),
        content_hash: content_fingerprint(content.as_bytes()),
        logical_path: "../../etc/passwd".into(), // tries to climb out of the jail
        content: content.into(),
    }));

    let err = provider
        .create(&spec)
        .await
        .expect_err("escape must fail closed");
    assert!(
        err.to_string().contains("escapes the sandbox root"),
        "unexpected: {err}"
    );
    let _ = provider.teardown("esc").await;
}

#[tokio::test]
async fn empty_spec_provisions_only_hand_tools() {
    let base = unique_base("empty");
    let provider = LocalSandboxProvider::new(&base);
    let env = provider.create(&SandboxSpec::new("plain")).await.unwrap();
    assert_eq!(env.tools().len(), 6); // hand tools only
    assert!(env.resources().is_empty());
    assert!(env.receipt().entries.is_empty());
    provider.teardown("plain").await.unwrap();
}
