//! Repository Git effects executed exclusively inside a container Sandbox.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use awaken_provisioning_contract as pc;
use awaken_sandbox_container::{SandboxControlService, SandboxControlServiceKind};
use awaken_sandbox_control::{
    CapabilityExpiresAtUnixMs, RepositoryGitCredentialQuery, SandboxControlRequest,
    SandboxControlResponse, SandboxControlSecret,
};
use awaken_sandbox_control_transport::REPOSITORY_GIT_CREDENTIAL_SOCKET_PATH;

const CREDENTIAL_OPERATION_TTL: Duration = Duration::from_secs(120);

fn repository_git_tls_env(
    remote_url: &str,
    credential: Option<&pc::RepositoryHttpBasicCredential>,
) -> Vec<pc::EnvVar> {
    let private_gateway = credential
        .is_some_and(pc::RepositoryHttpBasicCredential::is_gateway_capability)
        && url::Url::parse(remote_url)
            .ok()
            .is_some_and(|url| url.host_str().is_some_and(|host| host.ends_with(".svc")));
    if !private_gateway {
        return Vec::new();
    }
    vec![pc::EnvVar {
        name: "GIT_SSL_CAINFO".into(),
        value: pc::EnvValue::Inline {
            value: awaken_sandbox_container::REPOSITORY_GATEWAY_CA_PATH.into(),
        },
        visibility: pc::EnvVisibility::Process,
    }]
}

struct OperationRepositoryCredential {
    protocol: String,
    host: String,
    path: String,
    username: String,
    password: String,
    expires_at_unix_ms: u64,
}

impl OperationRepositoryCredential {
    fn new(
        remote_url: &str,
        credential: &pc::RepositoryHttpBasicCredential,
    ) -> Result<Self, pc::SandboxError> {
        let url = url::Url::parse(remote_url)
            .map_err(|_| pc::SandboxError::invalid("Repository transport URL is invalid"))?;
        if url.scheme() != "https" || url.query().is_some() || url.fragment().is_some() {
            return Err(pc::SandboxError::invalid(
                "credentialed Repository transport must use a canonical HTTPS URL",
            ));
        }
        let mut host = url
            .host_str()
            .ok_or_else(|| pc::SandboxError::invalid("Repository transport URL has no host"))?
            .to_owned();
        if let Some(port) = url.port() {
            host.push(':');
            host.push_str(&port.to_string());
        }
        let path = url.path().trim_start_matches('/').to_owned();
        if path.is_empty() {
            return Err(pc::SandboxError::invalid(
                "Repository transport URL has no path",
            ));
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| pc::SandboxError::backend("new", "system clock is before Unix epoch"))?;
        let expires_at_unix_ms = u64::try_from((now + CREDENTIAL_OPERATION_TTL).as_millis())
            .map_err(|_| {
                pc::SandboxError::backend("new", "Repository credential expiry overflow")
            })?;
        Ok(Self {
            protocol: url.scheme().to_owned(),
            host,
            path,
            username: credential.expose_username().to_owned(),
            password: credential.expose_password().to_owned(),
            expires_at_unix_ms,
        })
    }

    fn matches(&self, query: &RepositoryGitCredentialQuery) -> bool {
        query.protocol == self.protocol
            && query.host == self.host
            && query.path.trim_start_matches('/') == self.path
    }
}

#[async_trait::async_trait]
impl SandboxControlService for OperationRepositoryCredential {
    async fn handle(&self, request: SandboxControlRequest) -> SandboxControlResponse {
        let SandboxControlRequest::RepositoryGitCredentialGet { query } = request;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|duration| u64::try_from(duration.as_millis()).ok());
        if !self.matches(&query) || now.is_none_or(|now| now >= self.expires_at_unix_ms) {
            return SandboxControlResponse::Denied;
        }
        let Ok(username) = SandboxControlSecret::new(self.username.clone()) else {
            return SandboxControlResponse::Unavailable;
        };
        let Ok(password) = SandboxControlSecret::new(self.password.clone()) else {
            return SandboxControlResponse::Unavailable;
        };
        SandboxControlResponse::RepositoryGitCredential {
            username,
            password,
            expires_at_unix_ms: CapabilityExpiresAtUnixMs(self.expires_at_unix_ms),
        }
    }
}

async fn publish_credential(
    sandbox: &dyn awaken_sandbox_container::ContainerEnvironment,
    remote_url: &str,
    credential: Option<&pc::RepositoryHttpBasicCredential>,
) -> Result<
    Option<Box<dyn awaken_sandbox_container::PublishedSandboxControlService>>,
    pc::SandboxError,
> {
    let Some(credential) = credential else {
        return Ok(None);
    };
    let service = Arc::new(OperationRepositoryCredential::new(remote_url, credential)?);
    sandbox
        .publish_sandbox_control_service(
            SandboxControlServiceKind::RepositoryGitCredential,
            service,
        )
        .await
        .map(Some)
        .map_err(|_| {
            pc::SandboxError::transport("Sandbox Repository credential channel is unavailable")
        })
}

pub(super) async fn provision(
    sandbox: &dyn awaken_sandbox_container::ContainerEnvironment,
    publisher_bin: &str,
    plan: &pc::RepositoryRealizationPlan,
    credential: Option<&pc::RepositoryHttpBasicCredential>,
) -> Result<(), pc::SandboxError> {
    plan.validate_mount_path()?;
    let publication = publish_credential(sandbox, &plan.transport_url, credential).await?;
    let mut command = repository_clone_command(plan, publisher_bin, uuid::Uuid::new_v4())?;
    command
        .env
        .extend(repository_git_tls_env(&plan.transport_url, credential));
    let result = match sandbox.spawn(command).await {
        Ok(process) => process.wait().await,
        Err(error) => Err(error),
    };
    if let Some(publication) = publication {
        publication.close().await;
    }
    let status = result?;
    if status.code == Some(0) {
        sandbox.record_owned_path(&plan.mount_path)?;
        Ok(())
    } else {
        Err(pc::SandboxError::backend(
            "provision",
            format!("container Repository clone exited {:?}", status.code),
        ))
    }
}

fn repository_clone_command(
    plan: &pc::RepositoryRealizationPlan,
    publisher_bin: &str,
    stage_identity: uuid::Uuid,
) -> Result<pc::Command, pc::SandboxError> {
    let parent = plan
        .mount_path
        .rsplit_once('/')
        .map(|(parent, _)| parent)
        .filter(|parent| !parent.is_empty())
        .ok_or_else(|| pc::SandboxError::invalid("Repository destination has no sandbox parent"))?;
    let stage = format!(
        "{parent}/.awaken-repository-{}.stage",
        stage_identity.simple()
    );
    Ok(pc::Command {
        argv: vec![
            "sh".into(),
            "-c".into(),
            concat!(
                "set -eu; export GIT_TERMINAL_PROMPT=0; destination=$1; parent=$2; stage=$3; remote=$4; branch=$5; commit=$6; publisher=$7; helper=$8; socket=$9; ",
                "credential_helper=\"!$helper git-credential --socket $socket\"; stage_owned=0; ",
                "cleanup() { if test \"$stage_owned\" = 1; then rm -rf -- \"$stage\"; fi; }; trap cleanup EXIT; ",
                "is_occupied() { test -e \"$1\" || test -L \"$1\"; }; ",
                "is_exact() { candidate=$1; test -d \"$candidate\" && test ! -L \"$candidate\" || return 1; ",
                "test -d \"$candidate/.git\" && test ! -L \"$candidate/.git\" || return 1; ",
                "test \"$(git -C \"$candidate\" remote get-url origin)\" = \"$remote\" || return 1; ",
                "git -C \"$candidate\" rev-parse --verify 'HEAD^{commit}' >/dev/null 2>&1 || return 1; ",
                "if test -n \"$branch\"; then test \"$(git -C \"$candidate\" symbolic-ref --short HEAD)\" = \"$branch\" || return 1; fi; ",
                "if test -n \"$commit\"; then test \"$(git -C \"$candidate\" rev-parse HEAD)\" = \"$(git -C \"$candidate\" rev-parse \"$commit\")\" || return 1; fi; }; ",
                "if is_occupied \"$destination\"; then is_exact \"$destination\" && exit 0; echo 'Repository destination is occupied by a non-exact realization' >&2; exit 1; fi; ",
                "mkdir -p -- \"$parent\"; mkdir -m 700 -- \"$stage\"; stage_owned=1; ",
                "git -c credential.useHttpPath=true -c \"credential.helper=$credential_helper\" clone --no-checkout -- \"$remote\" \"$stage\"; ",
                "if test -n \"$branch\"; then git -C \"$stage\" checkout \"$branch\"; fi; ",
                "if test -n \"$commit\"; then git -C \"$stage\" checkout --detach \"$commit\"; else git -C \"$stage\" reset --hard HEAD; fi; is_exact \"$stage\"; ",
                "if is_occupied \"$destination\"; then is_exact \"$destination\" && exit 0; echo 'Repository destination became occupied before publication' >&2; exit 1; fi; ",
                "publish_status=0; \"$publisher\" repository-publish-noreplace \"$stage\" \"$destination\" || publish_status=$?; ",
                "if test \"$publish_status\" != 0; then is_exact \"$destination\" && exit 0; exit \"$publish_status\"; fi; stage_owned=0; is_exact \"$destination\""
            )
            .into(),
            "awaken-repo-clone".into(),
            plan.mount_path.clone(),
            parent.into(),
            stage,
            plan.transport_url.clone(),
            plan.initial_branch.clone().unwrap_or_default(),
            plan.initial_commit.clone().unwrap_or_default(),
            publisher_bin.into(),
            publisher_bin.into(),
            REPOSITORY_GIT_CREDENTIAL_SOCKET_PATH.into(),
        ],
        cwd: pc::WorkspaceLayout::ROOT.into(),
        env: Vec::new(),
        stdio: pc::Stdio::Null,
    })
}

pub(super) async fn push(
    sandbox: &dyn awaken_sandbox_container::ContainerEnvironment,
    helper_bin: &str,
    plan: &pc::RepositoryRealizationPlan,
    expectation: &pc::RepositoryPublicationExpectation,
    credential: Option<&pc::RepositoryHttpBasicCredential>,
) -> Result<pc::RepositoryPublicationReceipt, pc::RepositoryPublicationError> {
    plan.validate_mount_path()
        .map_err(pc::RepositoryPublicationError::Unavailable)?;
    expectation
        .validate()
        .map_err(pc::RepositoryPublicationError::Unavailable)?;
    if plan.access == pc::MountAccess::ReadOnly {
        return Err(pc::RepositoryPublicationError::Unavailable(
            pc::SandboxError::unauthorized("read-only repository cannot be published"),
        ));
    }
    let publication = publish_credential(sandbox, &plan.transport_url, credential)
        .await
        .map_err(pc::RepositoryPublicationError::Unavailable)?;
    let mut command = repository_push_command(plan, expectation, helper_bin);
    command
        .env
        .extend(repository_git_tls_env(&plan.transport_url, credential));
    let process = sandbox.spawn_agent_process(command).await;
    let result = match process {
        Ok(process) => process.process.wait().await,
        Err(error) => Err(error),
    };
    if let Some(publication) = publication {
        publication.close().await;
    }
    let status = result.map_err(pc::RepositoryPublicationError::Unavailable)?;
    match status.code {
        Some(0) => Ok(pc::RepositoryPublicationReceipt::new(plan, expectation)),
        // The in-Sandbox command deliberately emits no remote coordinate over
        // its process channel. Without typed conflict evidence the outer port
        // must classify a failed CAS as unavailable, never invent a permanent
        // rejection that could close a retryable Session effect.
        Some(42) => Err(pc::RepositoryPublicationError::Unavailable(
            pc::SandboxError::invalid("container Repository publication coordinate was rejected"),
        )),
        Some(43) => Err(pc::RepositoryPublicationError::Unavailable(
            pc::SandboxError::backend(
                "ls-remote",
                "container Repository remote observation was unavailable",
            ),
        )),
        code => Err(pc::RepositoryPublicationError::Unavailable(
            pc::SandboxError::backend("push", format!("container Repository push exited {code:?}")),
        )),
    }
}

fn repository_push_command(
    plan: &pc::RepositoryRealizationPlan,
    expectation: &pc::RepositoryPublicationExpectation,
    helper_bin: &str,
) -> pc::Command {
    pc::Command {
        argv: vec![
            "sh".into(),
            "-c".into(),
            concat!(
                "set -eu; export GIT_TERMINAL_PROMPT=0; repo=$1; remote=$2; branch=$3; commit=$4; prior=$5; helper=$6; socket=$7; ",
                "credential_helper=\"!$helper git-credential --socket $socket\"; ",
                "git check-ref-format --branch \"$branch\" >/dev/null 2>&1 || exit 42; ",
                "test \"$(git -C \"$repo\" symbolic-ref --quiet --short HEAD)\" = \"$branch\" || exit 42; ",
                "test \"$(git -C \"$repo\" rev-parse HEAD)\" = \"$commit\" || exit 42; ",
                "remote_refs=; for attempt in 1 2 3; do ",
                "if remote_refs=$(git -c credential.useHttpPath=true -c \"credential.helper=$credential_helper\" ls-remote --refs \"$remote\" \"refs/heads/$branch\"); then break; fi; ",
                "if test \"$attempt\" = 3; then exit 43; fi; sleep 1; done; ",
                "remote_commit=$(printf '%s\\n' \"$remote_refs\" | awk 'NR == 1 { print $1 }'); ",
                "if test \"$remote_commit\" = \"$commit\"; then exit 0; fi; ",
                "if test \"$remote_commit\" != \"$prior\"; then exit 42; fi; ",
                "git -c credential.useHttpPath=true -c \"credential.helper=$credential_helper\" -C \"$repo\" push --porcelain --force-with-lease=\"refs/heads/$branch:$prior\" \"$remote\" \"$commit:refs/heads/$branch\""
            )
            .into(),
            "awaken-repo-push".into(),
            plan.mount_path.clone(),
            plan.transport_url.clone(),
            expectation.branch.clone(),
            expectation.commit.clone(),
            expectation.expected_prior_commit.clone().unwrap_or_default(),
            helper_bin.into(),
            REPOSITORY_GIT_CREDENTIAL_SOCKET_PATH.into(),
        ],
        cwd: pc::WorkspaceLayout::ROOT.into(),
        env: Vec::new(),
        stdio: pc::Stdio::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_private_gateway_git_operations_select_the_internal_ca() {
        // Causes: C1=Gateway capability vs upstream/no credential; C2=private
        // Service DNS vs public host. Effects: E1=the single Git operation
        // receives the public internal CA; E2=all other Git processes retain
        // their normal system trust. Rules: T1(gateway,svc)->E1;
        // T2(gateway,public|upstream,any|absent,any)->E2.
        let gateway =
            pc::RepositoryHttpBasicCredential::gateway_capability("signed-capability".to_owned());
        let upstream =
            pc::RepositoryHttpBasicCredential::new("user".to_owned(), "password".to_owned());
        let private =
            repository_git_tls_env("https://gateway.awaken.svc:8444/git/repo", Some(&gateway));
        assert_eq!(private.len(), 1, "T1");
        assert_eq!(private[0].name, "GIT_SSL_CAINFO", "T1");
        assert_eq!(
            private[0].value,
            pc::EnvValue::Inline {
                value: awaken_sandbox_container::REPOSITORY_GATEWAY_CA_PATH.into()
            },
            "T1"
        );
        for (url, credential) in [
            ("https://gateway.example.com/git/repo", Some(&gateway)),
            ("https://gateway.awaken.svc:8444/git/repo", Some(&upstream)),
            ("https://gateway.awaken.svc:8444/git/repo", None),
        ] {
            assert!(
                repository_git_tls_env(url, credential).is_empty(),
                "T2 {url}"
            );
        }
    }

    fn plan() -> pc::RepositoryRealizationPlan {
        pc::RepositoryRealizationPlan {
            repository_id: "repository".into(),
            mount_path: "/workspace/source/repository".into(),
            source_remote_url: "https://example.invalid/repository".into(),
            transport_url: "https://example.invalid/repository".into(),
            initial_branch: Some("main".into()),
            initial_commit: None,
            access: pc::MountAccess::ReadWrite,
        }
    }

    #[test]
    fn clone_command_executes_network_git_in_sandbox_and_never_embeds_credentials() {
        let command =
            repository_clone_command(&plan(), "/opt/custom/awaken-sandbox", uuid::Uuid::nil())
                .expect("valid clone command");
        let script = &command.argv[2];
        assert!(script.contains("clone --no-checkout -- \"$remote\" \"$stage\""));
        assert!(script.contains("credential.helper=$credential_helper"));
        assert_eq!(command.argv[10], "/opt/custom/awaken-sandbox");
        assert_eq!(command.argv[11], "/opt/custom/awaken-sandbox");
        assert_eq!(command.argv[12], REPOSITORY_GIT_CREDENTIAL_SOCKET_PATH);
        assert!(!format!("{command:?}").contains("password"));
        assert!(!script.contains("rmdir -- \"$stage\""));
        assert_eq!(script.matches("rm -rf --").count(), 1);
        assert!(!script.contains("rm -rf -- \"$destination\""));
        assert!(script.contains("repository-publish-noreplace"));
    }

    #[test]
    fn push_command_is_exact_cas_and_executes_only_in_sandbox() {
        let expectation = pc::RepositoryPublicationExpectation {
            branch: "awf/work".into(),
            commit: "0123456789012345678901234567890123456789".into(),
            expected_prior_commit: Some("1111111111111111111111111111111111111111".into()),
        };
        let command = repository_push_command(&plan(), &expectation, "/opt/custom/awaken-sandbox");
        let script = &command.argv[2];
        assert!(script.contains("git check-ref-format --branch"));
        assert!(script.contains("ls-remote --refs"));
        assert!(script.contains("if remote_refs=$(git"));
        assert!(script.contains("exit 43"));
        assert!(script.contains("--force-with-lease="));
        assert!(script.contains("credential.helper=$credential_helper"));
        assert!(script.contains("exit 42"));
        assert_eq!(command.argv[9], "/opt/custom/awaken-sandbox");
        assert!(!format!("{command:?}").contains("password"));
    }

    #[cfg(unix)]
    #[test]
    fn push_observation_failure_cannot_masquerade_as_an_absent_remote_ref() {
        use std::os::unix::fs::PermissionsExt as _;

        // Cause/effect table: R1 first remote read fails, then exact prior is
        // observed -> one leased push; R2 every read fails -> unavailable (43),
        // zero push; R3 observed third commit -> coordinate refusal (42), zero
        // push; R4 absent create lease and absent ref -> one create; R5 desired
        // already remote -> replay without push. Network failure is never an
        // empty ref, especially when the expected prior commit is nonempty.
        let root = tempfile::tempdir().unwrap();
        let git = root.path().join("git");
        std::fs::write(
            &git,
            r#"#!/bin/sh
case " $* " in
  *" check-ref-format "*) exit 0 ;;
  *" symbolic-ref "*) printf 'awf/work\n'; exit 0 ;;
  *" rev-parse "*) printf '%s\n' "$FAKE_LOCAL_COMMIT"; exit 0 ;;
  *" ls-remote "*)
    count=$(cat "$FAKE_COUNT_FILE" 2>/dev/null || printf 0)
    count=$((count + 1))
    printf '%s\n' "$count" > "$FAKE_COUNT_FILE"
    if test "$count" -le "$FAKE_FAILS"; then exit 128; fi
    if test -n "$FAKE_REMOTE_SHA"; then printf '%s\trefs/heads/awf/work\n' "$FAKE_REMOTE_SHA"; fi
    exit 0 ;;
  *" push "*) : > "$FAKE_PUSH_FILE"; exit 0 ;;
esac
exit 99
"#,
        )
        .unwrap();
        std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).unwrap();
        let desired = "1".repeat(40);
        let prior = "2".repeat(40);
        let third = "3".repeat(40);
        for (rule, failures, remote, expected_prior, code, pushes, reads) in [
            ("R1", 1, prior.as_str(), Some(prior.as_str()), 0, true, 2),
            ("R2", 3, prior.as_str(), Some(prior.as_str()), 43, false, 3),
            ("R3", 0, third.as_str(), Some(prior.as_str()), 42, false, 1),
            ("R4", 0, "", None, 0, true, 1),
            ("R5", 0, desired.as_str(), Some(prior.as_str()), 0, false, 1),
        ] {
            let count_file = root.path().join(format!("{rule}-count"));
            let push_file = root.path().join(format!("{rule}-push"));
            let expectation = pc::RepositoryPublicationExpectation {
                branch: "awf/work".into(),
                commit: desired.clone(),
                expected_prior_commit: expected_prior.map(str::to_owned),
            };
            let command = repository_push_command(&plan(), &expectation, "/unused/helper");
            let output = std::process::Command::new("sh")
                .args(command.argv.iter().skip(1))
                .env(
                    "PATH",
                    format!(
                        "{}:{}",
                        root.path().display(),
                        std::env::var("PATH").unwrap_or_default()
                    ),
                )
                .env("FAKE_LOCAL_COMMIT", &desired)
                .env("FAKE_REMOTE_SHA", remote)
                .env("FAKE_FAILS", failures.to_string())
                .env("FAKE_COUNT_FILE", &count_file)
                .env("FAKE_PUSH_FILE", &push_file)
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(code), "{rule}: {output:?}");
            assert_eq!(push_file.exists(), pushes, "{rule}");
            assert_eq!(
                std::fs::read_to_string(count_file).unwrap().trim(),
                reads.to_string(),
                "{rule}"
            );
        }
    }

    #[tokio::test]
    async fn credential_service_is_exact_target_bound_and_redacted() {
        let credential =
            pc::RepositoryHttpBasicCredential::new("user".to_owned(), "secret".to_owned());
        let service = OperationRepositoryCredential::new(
            "https://github.example:8443/owner/repo.git",
            &credential,
        )
        .unwrap();
        let response = service
            .handle(SandboxControlRequest::RepositoryGitCredentialGet {
                query: RepositoryGitCredentialQuery {
                    protocol: "https".into(),
                    host: "github.example:8443".into(),
                    path: "owner/repo.git".into(),
                },
            })
            .await;
        assert!(matches!(
            response,
            SandboxControlResponse::RepositoryGitCredential { .. }
        ));
        let denied = service
            .handle(SandboxControlRequest::RepositoryGitCredentialGet {
                query: RepositoryGitCredentialQuery {
                    protocol: "https".into(),
                    host: "github.example:8443".into(),
                    path: "owner/other.git".into(),
                },
            })
            .await;
        assert!(matches!(denied, SandboxControlResponse::Denied));
        assert!(!format!("{response:?}").contains("secret"));
    }
}
