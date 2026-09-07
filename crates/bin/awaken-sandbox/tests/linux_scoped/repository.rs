use super::*;
use pc::RepositoryRealizer as _;

fn plan(url: String) -> pc::RepositoryRealizationPlan {
    pc::RepositoryRealizationPlan {
        repository_id: "repo".into(),
        mount_path: "/workspace/repo".into(),
        source_remote_url: url.clone(),
        transport_url: url,
        initial_branch: Some("main".into()),
        initial_commit: None,
        access: pc::MountAccess::ReadWrite,
    }
}

fn git(root: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .current_dir(root)
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn helper_fixture(root: &Path, body: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let helper = root.join("helper");
    std::fs::write(&helper, format!(
        "#!/usr/bin/python3\nimport os,sys,time,json\nif sys.argv[1]=='scoped-exec':\n os.execv({binary:?},[{binary:?}]+sys.argv[1:])\nrequest_bytes=sys.stdin.buffer.read()\nassert 'REPOSITORY_AGENT_ONLY' not in os.environ\n{body}\n",
        binary = sandbox_binary(),
    )).unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
    helper
}

#[tokio::test]
async fn scoped_repository_reuses_clone_publication_and_exact_resource_paths() {
    // R1 live bound scope + valid repo -> clone and replay through actual
    // helper/Git; R2 same publication -> idempotent exact receipt; R3 stale CAS
    // -> typed rejection, remote unchanged. R4 Agent shell env
    // never affects internal Worker-side helper. Credentials here are absent:
    // local bare transport is test-support, NOT real cloud/vendor qualification.
    let base = tempfile::tempdir().unwrap();
    let remote = base.path().join("remote.git");
    git(base.path(), &["init", "--bare", remote.to_str().unwrap()]);
    let seed = base.path().join("seed");
    std::fs::create_dir(&seed).unwrap();
    git(&seed, &["init", "-b", "main"]);
    std::fs::write(seed.join("file"), b"exact\0bytes\xff").unwrap();
    git(&seed, &["add", "file"]);
    git(&seed, &["commit", "-m", "seed"]);
    git(&seed, &["push", remote.to_str().unwrap(), "main"]);
    let original = git(&seed, &["rev-parse", "HEAD"]);
    let mut specification = spec();
    specification.env.push(pc::EnvVar {
        name: "BASH_ENV".into(),
        value: pc::EnvValue::Inline {
            value: "/nonexistent-agent-only".into(),
        },
        visibility: pc::EnvVisibility::Process,
    });
    let provider = LocalProvider::trusted_host(base.path());
    let sandbox = provider
        .create_sandbox_for_effect(&specification, &fence("create"), None)
        .await
        .unwrap();
    sandbox
        .install_scoped_execution(
            Path::new("/usr/bin/bwrap"),
            Path::new(sandbox_binary()),
            Authority::live(),
        )
        .unwrap();
    let plan = plan(remote.to_str().unwrap().into());
    sandbox.realize_repository(&plan, None).await.unwrap();
    sandbox.realize_repository(&plan, None).await.unwrap();
    let checkout = sandbox.workspace_path().join("workspace/repo");
    assert_eq!(
        std::fs::read(checkout.join("file")).unwrap(),
        b"exact\0bytes\xff"
    );
    std::fs::write(checkout.join("file"), b"published without newline").unwrap();
    git(&checkout, &["add", "file"]);
    git(&checkout, &["commit", "-m", "publish"]);
    let desired = git(&checkout, &["rev-parse", "HEAD"]);
    let expectation = pc::RepositoryPublicationExpectation {
        branch: "main".into(),
        commit: desired.clone(),
        expected_prior_commit: Some(original.clone()),
    };
    let receipt = sandbox
        .publish_repository(&plan, &expectation, None)
        .await
        .unwrap();
    assert_eq!(
        receipt,
        pc::RepositoryPublicationReceipt::new(&plan, &expectation)
    );
    assert_eq!(
        sandbox
            .publish_repository(&plan, &expectation, None)
            .await
            .unwrap(),
        receipt
    );
    let stale = pc::RepositoryPublicationExpectation {
        commit: original,
        expected_prior_commit: None,
        ..expectation
    };
    // Restore the proposed HEAD so rejection tests remote CAS, not local HEAD.
    git(&checkout, &["reset", "--hard", &stale.commit]);
    assert!(matches!(
        sandbox.publish_repository(&plan, &stale, None).await,
        Err(pc::RepositoryPublicationError::Rejected(_))
    ));
    assert_eq!(git(&remote, &["rev-parse", "refs/heads/main"]), desired);
    assert_eq!(
        std::fs::read(base.path().join("scoped-provider/mnt/session/file")).unwrap(),
        b"exact\0file\xff"
    );
    let temporary = sandbox
        .workspace_path()
        .join("workspace/.awaken/repository-tmp");
    assert!(temporary.is_dir());
    assert_eq!(std::fs::read_dir(temporary).unwrap().count(), 0);
    dispose(&sandbox).await;
}

#[tokio::test]
async fn repository_reply_does_not_complete_before_tree_exit() {
    // R5 valid reply + EOF with leader still live -> remain pending; explicit
    // stop -> no successful Repository receipt. Handshake file is emitted only
    // AFTER stdout/stderr close, so a blocked read cannot satisfy this oracle.
    // The wrapper reuses the real scoped-exec role and supplies a controlled
    // business effect. This is the Rust counterpart of LocalRepositoryEffect R2.
    let base = tempfile::tempdir().unwrap();
    let reached = base.path().join("reply-sent");
    let wrapper = helper_fixture(
        base.path(),
        &format!(
            "os.write(1,b'\"Realized\"')\nos.close(1)\nos.close(2)\nopen({:?},'w').close()\ntime.sleep(60)",
            reached.to_str().unwrap(),
        ),
    );
    let provider = LocalProvider::trusted_host(base.path());
    let mut specification = spec();
    specification.env.push(pc::EnvVar {
        name: "REPOSITORY_AGENT_ONLY".into(),
        value: pc::EnvValue::Inline {
            value: "must-not-reach-internal-helper".into(),
        },
        visibility: pc::EnvVisibility::Process,
    });
    let sandbox = Arc::new(
        provider
            .create_sandbox_for_effect(&specification, &fence("create"), None)
            .await
            .unwrap(),
    );
    sandbox
        .install_scoped_execution(Path::new("/usr/bin/bwrap"), &wrapper, Authority::live())
        .unwrap();
    let running = {
        let sandbox = sandbox.clone();
        tokio::spawn(async move {
            sandbox
                .realize_repository(&plan("https://example.invalid/repo".into()), None)
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        while !reached.exists() {
            assert!(
                !running.is_finished(),
                "effect ended before its reply barrier"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !running.is_finished(),
        "stdout alone accepted as a complete tree receipt"
    );
    sandbox.stop_scoped_execution().await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(5), running)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    dispose(&sandbox).await;
}

#[tokio::test]
async fn repository_response_failures_are_bounded_and_operation_specific() {
    // Cause/effect R7-R11: complete tree exit is necessary, not sufficient.
    // Malformed/oversize bytes, wrong operation, nonzero status and unavailable
    // workflow each fail. Parse/status errors never echo synthetic payloads.
    // Successful exact realization is a positive control for the same transport.
    for (body, expected) in [
        (
            "os.write(1,b'synthetic-secret')",
            Some("repository response invalid"),
        ),
        (
            "sys.stdout.write('x' * (1024 * 1024 + 1))",
            Some("repository response too large"),
        ),
        (
            "os.write(1,b'\"Realized\"');sys.exit(1)",
            Some("repository helper failed"),
        ),
        (
            "os.write(1,b'{\"Rejected\":{\"reason\":\"remote_ref_absent\"}}')",
            Some("repository realization returned another operation's receipt"),
        ),
        (
            "os.write(1,b'{\"Unavailable\":\"controlled failure\"}')",
            Some("controlled failure"),
        ),
        ("os.write(1,b'\"Realized\"')", None),
    ] {
        let base = tempfile::tempdir().unwrap();
        let helper = helper_fixture(base.path(), body);
        let provider = LocalProvider::trusted_host(base.path());
        let sandbox = provider
            .create_sandbox_for_effect(&spec(), &fence("create"), None)
            .await
            .unwrap();
        sandbox
            .install_scoped_execution(Path::new("/usr/bin/bwrap"), &helper, Authority::live())
            .unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            sandbox.realize_repository(&plan("https://example.invalid/repo".into()), None),
        )
        .await
        .unwrap();
        match expected {
            Some(expected) => {
                let error = result.unwrap_err().to_string();
                assert!(error.contains(expected), "{error}");
                assert!(!error.contains("synthetic-secret"));
            }
            None => result.unwrap(),
        }
        dispose(&sandbox).await;
    }
}

#[tokio::test]
async fn repository_credentials_cross_only_the_private_command_environment() {
    // R12 none/upstream/gateway -> exact existing credential keys in the real
    // scoped child, discriminator-only request and no credential bytes in WAL.
    // Synthetic transport qualification, not upstream/provider login proof.
    let upstream = pc::RepositoryHttpBasicCredential::new(
        "fixture-private-user".to_owned(),
        "fixture-private-password".to_owned(),
    );
    let gateway = pc::RepositoryHttpBasicCredential::gateway_capability(
        "fixture-gateway-capability".to_owned(),
    );
    for (credential, kind) in [
        (None, "None"),
        (Some(&upstream), "Upstream"),
        (Some(&gateway), "Gateway"),
    ] {
        let base = tempfile::tempdir().unwrap();
        let username = credential.map_or("", |value| value.expose_username());
        let password = credential.map_or("", |value| value.expose_password());
        let helper = helper_fixture(
            base.path(),
            &format!(
                "assert os.environ['AWAKEN_GIT_CREDENTIAL_USERNAME']=={username:?}\nassert os.environ['AWAKEN_GIT_CREDENTIAL_PASSWORD']=={password:?}\nassert json.loads(request_bytes)['credential']=={kind:?}\nassert not {password:?} or {password:?}.encode() not in request_bytes\nos.write(1,b'\"Realized\"')",
            ),
        );
        let provider = LocalProvider::trusted_host(base.path());
        let sandbox = provider
            .create_sandbox_for_effect(&spec(), &fence("create"), None)
            .await
            .unwrap();
        sandbox
            .install_scoped_execution(Path::new("/usr/bin/bwrap"), &helper, Authority::live())
            .unwrap();
        sandbox
            .realize_repository(&plan("https://example.invalid/repo".into()), credential)
            .await
            .unwrap();
        let evidence = marker(base.path()).to_string();
        if !password.is_empty() {
            assert!(!evidence.contains(password));
        }
        dispose(&sandbox).await;
    }
}

#[tokio::test]
async fn repository_publication_accepts_only_exact_typed_evidence() {
    // R13 complete successful scope + exact receipt -> success; different
    // Repository coordinate, wrong operation or unavailable -> retryable error.
    // R14 genuine frozen-prior conflict -> typed rejection; desired commit
    // masquerading as a conflict -> unavailable, never a permanent rejection.
    // The fake effect isolates transport validation; actual push/CAS is above.
    let plan = plan("https://example.invalid/repo".into());
    let expectation = pc::RepositoryPublicationExpectation {
        branch: "main".into(),
        commit: "a".repeat(40),
        expected_prior_commit: Some("b".repeat(40)),
    };
    let wrong_plan = pc::RepositoryRealizationPlan {
        repository_id: "another-repository".into(),
        ..plan.clone()
    };
    let exact = pc::RepositoryPublicationReceipt::new(&plan, &expectation);
    for (outcome, success, rejected) in [
        (serde_json::json!({"Published": exact}), true, false),
        (
            serde_json::json!({"Published":
            pc::RepositoryPublicationReceipt::new(&wrong_plan, &expectation)}),
            false,
            false,
        ),
        (serde_json::json!("Realized"), false, false),
        (
            serde_json::json!({"Unavailable": "controlled failure"}),
            false,
            false,
        ),
        (
            serde_json::json!({"Rejected": pc::RepositoryPublicationRejection::RemoteRefAbsent}),
            false,
            true,
        ),
        (
            serde_json::json!({"Rejected": pc::RepositoryPublicationRejection::RemoteRefChanged {
                observed_commit: expectation.commit.clone(),
            }}),
            false,
            false,
        ),
    ] {
        let base = tempfile::tempdir().unwrap();
        let helper = helper_fixture(
            base.path(),
            &format!("sys.stdout.write({:?})", outcome.to_string()),
        );
        let provider = LocalProvider::trusted_host(base.path());
        let sandbox = provider
            .create_sandbox_for_effect(&spec(), &fence("create"), None)
            .await
            .unwrap();
        sandbox
            .install_scoped_execution(Path::new("/usr/bin/bwrap"), &helper, Authority::live())
            .unwrap();
        let result = sandbox.publish_repository(&plan, &expectation, None).await;
        assert_eq!(result.is_ok(), success, "{result:?}");
        assert_eq!(
            matches!(result, Err(pc::RepositoryPublicationError::Rejected(_))),
            rejected
        );
        if success {
            assert_eq!(result.unwrap(), exact);
        }
        dispose(&sandbox).await;
    }
}
