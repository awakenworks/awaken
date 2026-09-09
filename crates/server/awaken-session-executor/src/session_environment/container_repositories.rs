//! Repository Git effects executed exclusively inside a container Sandbox.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use awaken_provisioning_contract as pc;
use awaken_sandbox_container::{SandboxControlService, SandboxControlServiceKind};
use awaken_sandbox_control::{
    CapabilityExpiresAtUnixMs, REPOSITORY_GIT_CREDENTIAL_SOCKET_PATH, RepositoryGitCredentialQuery,
    SandboxControlRequest, SandboxControlResponse, SandboxControlSecret,
};

const CREDENTIAL_OPERATION_TTL: Duration = Duration::from_secs(120);

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
            .map_err(|_| pc::SandboxError::new("Repository transport URL is invalid"))?;
        if url.scheme() != "https" || url.query().is_some() || url.fragment().is_some() {
            return Err(pc::SandboxError::new(
                "credentialed Repository transport must use a canonical HTTPS URL",
            ));
        }
        let mut host = url
            .host_str()
            .ok_or_else(|| pc::SandboxError::new("Repository transport URL has no host"))?
            .to_owned();
        if let Some(port) = url.port() {
            host.push(':');
            host.push_str(&port.to_string());
        }
        let path = url.path().trim_start_matches('/').to_owned();
        if path.is_empty() {
            return Err(pc::SandboxError::new(
                "Repository transport URL has no path",
            ));
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| pc::SandboxError::new("system clock is before Unix epoch"))?;
        let expires_at_unix_ms = u64::try_from((now + CREDENTIAL_OPERATION_TTL).as_millis())
            .map_err(|_| pc::SandboxError::new("Repository credential expiry overflow"))?;
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
        .map_err(|_| pc::SandboxError::new("Sandbox Repository credential channel is unavailable"))
}

pub(super) async fn provision(
    sandbox: &dyn awaken_sandbox_container::ContainerEnvironment,
    publisher_bin: &str,
    plan: &pc::RepositoryRealizationPlan,
    credential: Option<&pc::RepositoryHttpBasicCredential>,
) -> Result<(), pc::SandboxError> {
    plan.validate_mount_path()?;
    let publication = publish_credential(sandbox, &plan.transport_url, credential).await?;
    let command = repository_clone_command(plan, publisher_bin, uuid::Uuid::new_v4())?;
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
        Err(pc::SandboxError::new(format!(
            "container Repository clone exited {:?}",
            status.code
        )))
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
        .ok_or_else(|| pc::SandboxError::new("Repository destination has no sandbox parent"))?;
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
            pc::SandboxError::new("read-only repository cannot be published"),
        ));
    }
    let publication = publish_credential(sandbox, &plan.transport_url, credential)
        .await
        .map_err(pc::RepositoryPublicationError::Unavailable)?;
    let process = sandbox
        .spawn_agent_process(repository_push_command(plan, expectation, helper_bin))
        .await;
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
            pc::SandboxError::new("container Repository publication coordinate was rejected"),
        )),
        code => Err(pc::RepositoryPublicationError::Unavailable(
            pc::SandboxError::new(format!("container Repository push exited {code:?}")),
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
                "remote_commit=$(git -c credential.useHttpPath=true -c \"credential.helper=$credential_helper\" ls-remote --refs \"$remote\" \"refs/heads/$branch\" | awk 'NR == 1 { print $1 }'); ",
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
        assert!(script.contains("--force-with-lease="));
        assert!(script.contains("credential.helper=$credential_helper"));
        assert!(script.contains("exit 42"));
        assert_eq!(command.argv[9], "/opt/custom/awaken-sandbox");
        assert!(!format!("{command:?}").contains("password"));
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
