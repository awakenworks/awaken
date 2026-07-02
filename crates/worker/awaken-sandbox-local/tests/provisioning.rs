//! End-to-end provisioning tests (ADR-0035, step 1).
//!
//! Exercise the public seam only: build a [`SandboxSpec`] with typed mounts, let
//! `LocalSandboxProvider::create` provision an [`Environment`], and assert the
//! whole capability surface — skill tool invocation, realized resources, the
//! host-side receipt, forward-compat, fail-closed integrity, and the G13 empty
//! state — behaves as designed.

use std::path::PathBuf;

use awaken_runtime_contract::llm::ToolCall;
use awaken_sandbox_local::{
    LocalSandboxProvider, Mount, ProvisionKind, ResourceMount, SandboxProvider, SandboxSpec,
    SkillMount, content_fingerprint,
};

fn unique_base(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("awaken-sbx-e2e-{}-{tag}", std::process::id()))
}

fn call(tool_id: &str) -> ToolCall {
    ToolCall {
        call_id: "c1".into(),
        tool_id: tool_id.into(),
        arguments: serde_json::json!({}),
    }
}

#[tokio::test]
async fn provision_skill_and_resource_end_to_end() {
    let base = unique_base("ok");
    let provider = LocalSandboxProvider::new(&base);

    let body = "# Deploy\nRun the deploy checklist before shipping.";
    let res_content = "alpha=1\nbeta=2\n";

    let mut spec = SandboxSpec::new("e2e")
        .with_mount(Mount::Skill(SkillMount {
            id: "deploy".into(),
            version: 1,
            content_hash: content_fingerprint(body.as_bytes()),
            name: "deploy".into(),
            description: "Run a deploy".into(),
            body: body.into(),
        }))
        .with_mount(Mount::Resource(ResourceMount {
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

    // Capability surface: the six hand tools plus the provisioned skill tool.
    let tools = env.tools();
    let ids: Vec<String> = tools.iter().map(|t| t.id().to_string()).collect();
    for hand in ["read", "write", "edit", "glob", "grep", "bash"] {
        assert!(ids.contains(&hand.to_string()), "missing hand tool {hand}");
    }
    assert!(
        ids.contains(&"skill__deploy".to_string()),
        "skill tool not on the capability surface: {ids:?}"
    );
    // `hand_tools()` is unchanged — only the six isolation tools.
    assert_eq!(env.hand_tools().len(), 6);

    // Activating the skill returns its body (progressive disclosure) and, per G13,
    // carries empty runtime state.
    let skill = tools
        .iter()
        .find(|t| t.id() == "skill__deploy")
        .expect("skill tool present");
    let out = skill.invoke(call("skill__deploy")).await.unwrap();
    assert_eq!(out.content, body);
    assert!(!out.is_error);
    assert!(
        out.state.is_empty(),
        "environment tool must not author state (G13)"
    );

    // The resource is realized under the read-only `.mnt/` root and exposed by
    // logical path only — no host absolute path crosses the boundary (G3).
    let refs = env.resources();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].id, "cfg");
    assert_eq!(refs[0].logical_path, ".mnt/config/app.env");
    assert!(
        !refs[0].logical_path.starts_with('/'),
        "resource ref must be logical, not a host path"
    );
    // The bytes really landed under the environment root.
    let realized = base.join("e2e").join(".mnt/config/app.env");
    assert_eq!(std::fs::read_to_string(&realized).unwrap(), res_content);

    // The receipt is the host-side pin: both provisioned entries, content-addressed.
    let receipt = env.receipt();
    assert_eq!(receipt.entries.len(), 2);
    assert!(
        receipt.entries.iter().any(|e| e.id == "deploy"
            && e.kind == ProvisionKind::Skill
            && !e.content_hash.is_empty())
    );
    assert!(
        receipt
            .entries
            .iter()
            .any(|e| e.id == "cfg" && e.kind == ProvisionKind::Resource)
    );

    provider.teardown("e2e").await.unwrap();
}

#[tokio::test]
async fn reprovision_from_same_spec_yields_identical_receipt() {
    // Cold replay baseline: the same spec provisions an identical (content-addressed)
    // environment, so the receipt — the pin — is stable across provisions.
    let base = unique_base("replay");
    let provider = LocalSandboxProvider::new(&base);
    let body = "# Skill\nbody text";
    let spec = SandboxSpec::new("r").with_mount(Mount::Skill(SkillMount {
        id: "s".into(),
        version: 3,
        content_hash: content_fingerprint(body.as_bytes()),
        name: "s".into(),
        description: "d".into(),
        body: body.into(),
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
    let spec = SandboxSpec::new("bad").with_mount(Mount::Skill(SkillMount {
        id: "x".into(),
        version: 1,
        content_hash: "deadbeef".into(), // does not match the body
        name: "x".into(),
        description: String::new(),
        body: "the real body".into(),
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
