use super::*;

const CHILD: &str = "subprocess::host_identity_tests::identity_child";
const STAGE: &str = "AWAKEN_IDENTITY_TEST_STAGE";

#[test]
fn trusted_host_process_uses_git_identity_without_ambient_tokens() {
    // Cause/effect graph: C1 two concurrent launches share one host identity;
    // C2 HOME has Git config and a credential helper; C3 ambient tokens/askpass
    // are hostile; C4 projected HOME conflicts. Effects: E1 both real children
    // read host Git config/helper; E2 host identity wins; E3 unrelated ambient
    // env is absent. Rules H1=C1+C2=>E1; H2=C3+C4=>E2+E3.
    // The helper is Git's actual file credential helper against example.invalid,
    // never a network request or a read of the operator's real credential files.
    // Each environment is set on a child Command, never the parallel test process.
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join(".gitconfig"),
        "[user]\nname = Trusted Host Fixture\n[credential]\nhelper =\nhelper = store\n",
    )
    .unwrap();
    std::fs::write(
        home.path().join(".git-credentials"),
        "https://fixture-user:fixture-password@example.invalid\n",
    )
    .unwrap();
    let children: Vec<_> = (0..2)
        .map(|_| {
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--ignored", "--exact", CHILD, "--nocapture"])
                .env(STAGE, "host")
                .env("HOME", home.path())
                .env("USERPROFILE", home.path())
                .env("SSH_AUTH_SOCK", "fixture-ssh-socket")
                .env("AWAKEN_AMBIENT_IDENTITY_SENTINEL", "must-not-cross")
                .env("OPENAI_API_KEY", "fixture-ambient-token")
                .env("GIT_ASKPASS", "must-not-execute")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    for child in children {
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("IDENTITY_CHECK_OK"));
    }
    assert_eq!(
        std::fs::read_to_string(home.path().join(".git-credentials")).unwrap(),
        "https://fixture-user:fixture-password@example.invalid\n",
        "H1 concurrent reads never rewrite shared identity"
    );
}

#[test]
#[ignore = "subprocess participant, invoked by the host identity decision table"]
fn identity_child() {
    match std::env::var(STAGE).unwrap().as_str() {
        "host" => {
            tokio::runtime::Runtime::new().unwrap().block_on(async {
                use tokio::io::AsyncReadExt;
                let mut launch = AcpLaunch::custom(
                    vec![
                        std::env::current_exe()
                            .unwrap()
                            .to_string_lossy()
                            .into_owned(),
                        "--ignored".into(),
                        "--exact".into(),
                        CHILD.into(),
                        "--nocapture".into(),
                    ],
                    vec![
                        (STAGE.into(), "agent".into()),
                        ("HOME".into(), "wrong-home".into()),
                    ],
                );
                launch.identity = AcpLaunchIdentity::BackendOwned;
                let session = spawn(&launch, awaken_protocol_acp::Codec::Newline, None)
                    .await
                    .unwrap();
                let mut channel = session.channel;
                let mut output = String::new();
                channel.read_to_string(&mut output).await.unwrap();
                assert_eq!(session.process.wait().await.unwrap().code, Some(0));
                assert!(output.contains("IDENTITY_CHECK_OK"));
                println!("IDENTITY_CHECK_OK");
            });
        }
        "agent" => {
            for key in [
                "AWAKEN_AMBIENT_IDENTITY_SENTINEL",
                "OPENAI_API_KEY",
                "GIT_ASKPASS",
            ] {
                assert!(
                    std::env::var_os(key).is_none(),
                    "unexpected ambient key: {key}"
                );
            }
            assert_eq!(
                std::env::var("SSH_AUTH_SOCK").unwrap(),
                "fixture-ssh-socket"
            );
            let name = std::process::Command::new("git")
                .args(["config", "--global", "--get", "user.name"])
                .output()
                .unwrap();
            assert!(name.status.success());
            assert_eq!(
                String::from_utf8(name.stdout).unwrap().trim(),
                "Trusted Host Fixture"
            );
            let mut helper = std::process::Command::new("git")
                .args(["credential", "fill"])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            use std::io::Write;
            helper
                .stdin
                .take()
                .unwrap()
                .write_all(b"protocol=https\nhost=example.invalid\n\n")
                .unwrap();
            let result = helper.wait_with_output().unwrap();
            assert!(result.status.success());
            let credential = String::from_utf8(result.stdout).unwrap();
            assert!(
                credential
                    .lines()
                    .any(|line| line == "username=fixture-user")
            );
            assert!(
                credential
                    .lines()
                    .any(|line| line == "password=fixture-password")
            );
            println!("IDENTITY_CHECK_OK");
        }
        other => panic!("unknown subprocess role {other}"),
    }
}
