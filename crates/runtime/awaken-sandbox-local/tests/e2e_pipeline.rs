//! ADR-0041 end-to-end: the full pipeline a host drives —
//! declare an environment → admit it (`EnvironmentSoundness`) → validate against a
//! provider's capabilities (`prepare_environment`) → realize (`create`) → run an
//! *opaque* process (`sh` stands in for Claude Code) → collect artifacts → dispose.
//! Also covers capability-driven tier selection between the Workdir and Namespace
//! providers.

use std::sync::Arc;

use awaken_provisioning_contract as pc;
use awaken_provisioning_contract::SandboxProvider;
use awaken_sandbox_local::{FileStore, InMemoryFileStore, LocalProvider, NamespaceProvider};

/// A declared environment (control plane) projected to admission facts + a runnable
/// spec. Mirrors `EnvironmentKind::Sandbox` with a seeded input file and one env var.
async fn declared() -> (pc::EnvironmentDecl, pc::SandboxSpec, Arc<InMemoryFileStore>) {
    let store = Arc::new(InMemoryFileStore::new());
    let file_id = store.put(b"input-corpus").await.unwrap();

    let decl = pc::EnvironmentDecl {
        summary: "a coding sandbox for e2e".into(),
        kind: "sandbox".into(),
        required_field: None,
        writable_base: false,
        max_concurrency: Some(4),
        env_keys: vec!["TZ".into()],
    };

    let spec = pc::SandboxSpec {
        scope: "e2e-1".into(),
        isolation: pc::IsolationClass::Workdir,
        mounts: vec![pc::MountRequirement {
            mount_id: "corpus".into(),
            source: pc::MountSource::File {
                file_id: file_id.clone(),
                content_hash: Some(file_id),
            },
            mount_path: "/workspace/corpus.txt".into(),
            access: pc::MountAccess::ReadWrite,
            lifetime: pc::MountLifetime::PerRun,
            required: true,
        }],
        env: vec![pc::EnvVar {
            name: "TZ".into(),
            value: pc::EnvValue::Inline {
                value: "UTC".into(),
            },
            visibility: pc::EnvVisibility::Process,
        }],
        network: pc::NetworkPolicy::Unrestricted,
        outputs_path: "/mnt/session/outputs".into(),
        limits: Default::default(),
        lease_ttl_secs: None,
        extra: None,
    };
    (decl, spec, store)
}

#[tokio::test]
async fn full_declare_admit_prepare_realize_execute_retrieve() {
    let tmp = tempfile::tempdir().unwrap();
    let (decl, spec, store) = declared().await;

    // 1. Admission (control plane): a well-formed declaration is accepted.
    pc::check_environment_soundness(&decl).expect("declaration is sound");

    // 2. Build the provider (Workdir tier) with the content-addressed store.
    let provider = LocalProvider::new(tmp.path()).with_file_store(store);

    // 3. Validate the spec against the backend's capabilities (fail-closed).
    let plan = pc::prepare_environment(&spec, &provider.capabilities()).expect("spec fits backend");
    assert_eq!(plan.mounts.len(), 1);
    assert_eq!(plan.outputs_path, "/mnt/session/outputs");

    // 4. Realize the environment (fan out the mount from the store).
    let sandbox = provider.create(&spec).await.unwrap();
    assert_eq!(sandbox.realized().len(), 1);

    // 5. Run an opaque process (Claude Code stand-in): consume the input, produce
    //    an artifact under the outputs dir, and use the injected env var.
    let proc = sandbox
        .spawn({
            let mut c = pc::Command::new([
                "sh",
                "-c",
                r#"printf '%s @ %s' "$(cat workspace/corpus.txt)" "$TZ" > "$AWAKEN_OUTPUTS_DIR/report.txt""#,
            ]);
            c.stdio = pc::Stdio::Null;
            c
        })
        .await
        .unwrap();
    assert_eq!(proc.wait().await.unwrap().code, Some(0));

    // 6. Retrieve the artifact.
    let artifacts = sandbox.artifacts().await.unwrap();
    let report = artifacts
        .iter()
        .find(|a| a.path.ends_with("/report.txt"))
        .expect("agent wrote a report");
    assert_eq!(
        sandbox.read_artifact(&report.id).await.unwrap(),
        b"input-corpus @ UTC"
    );

    // 7. Tear down.
    sandbox.dispose().await.unwrap();
    assert!(matches!(
        sandbox.status().await.unwrap(),
        pc::SandboxStatus::Terminated
    ));
}

#[tokio::test]
async fn admission_rejects_a_reserved_env_key_before_any_provisioning() {
    let (mut decl, _spec, _store) = declared().await;
    decl.env_keys = vec!["PATH".into()]; // runtime-owned
    assert!(pc::check_environment_soundness(&decl).is_err());
}

#[tokio::test]
async fn capability_driven_tier_selection() {
    let tmp = tempfile::tempdir().unwrap();
    // A workload that needs OS isolation (to host an opaque agent).
    let mut spec = declared().await.1;
    spec.scope = "e2e-iso".into();
    spec.isolation = pc::IsolationClass::Namespace;
    // read-only mount now allowed because the chosen tier enforces it
    spec.mounts[0].access = pc::MountAccess::ReadOnly;

    let workdir = LocalProvider::new(tmp.path().join("a"));
    let namespace = NamespaceProvider::new(tmp.path().join("b")).with_blob("x", b"y".to_vec()); // seed unused; just constructs

    // The Workdir tier is not tool-transparent and cannot enforce read-only, so it
    // is (correctly) refused; the Namespace tier accepts the spec.
    assert!(!workdir.capabilities().tool_transparent);
    assert!(pc::prepare_environment(&spec, &workdir.capabilities()).is_err());

    assert!(namespace.capabilities().tool_transparent);
    assert!(pc::prepare_environment(&spec, &namespace.capabilities()).is_ok());
    // realize succeeds without executing (bwrap only needed at spawn time)
    let store = declared().await.2;
    let namespace = NamespaceProvider::new(tmp.path().join("c")).with_file_store(store);
    assert!(namespace.create(&spec).await.is_ok());
}
