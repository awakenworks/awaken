//! Repository Git effects executed exclusively inside a container Sandbox.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use awaken_provisioning_contract as pc;
use awaken_sandbox_control::{
    CapabilityExpiresAtUnixMs, REPOSITORY_GIT_CREDENTIAL_SOCKET_PATH, RepositoryGitCredentialQuery,
    SandboxControlRequest, SandboxControlResponse, SandboxControlSecret, SandboxControlService,
    SandboxControlServiceKind,
};
use tokio::io::AsyncReadExt;

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
        Ok(process) => {
            let mut observation = String::new();
            let mut channel = process.channel.take(64);
            let (read, status) = tokio::join!(
                channel.read_to_string(&mut observation),
                process.process.wait()
            );
            match read {
                Ok(_) => status.map(|status| (status, observation)),
                Err(error) => Err(pc::SandboxError::backend(
                    "push-observation",
                    error.to_string(),
                )),
            }
        }
        Err(error) => Err(error),
    };
    if let Some(publication) = publication {
        publication.close().await;
    }
    let (status, observation) = result.map_err(pc::RepositoryPublicationError::Unavailable)?;
    match status.code {
        Some(0) if observation.is_empty() => {
            Ok(pc::RepositoryPublicationReceipt::new(plan, expectation))
        }
        Some(0) => Err(pc::RepositoryPublicationError::Unavailable(
            pc::SandboxError::invalid("container Repository success emitted unexpected output"),
        )),
        Some(42) => match decode_publication_rejection(&observation, expectation) {
            Ok(rejection) => Err(pc::RepositoryPublicationError::Rejected(rejection)),
            Err(error) => Err(pc::RepositoryPublicationError::Unavailable(error)),
        },
        Some(43) => Err(pc::RepositoryPublicationError::Unavailable(
            pc::SandboxError::backend(
                "repository-publication",
                "container Repository observation or push remained unavailable",
            ),
        )),
        code => Err(pc::RepositoryPublicationError::Unavailable(
            pc::SandboxError::backend("push", format!("container Repository push exited {code:?}")),
        )),
    }
}

fn decode_publication_rejection(
    observation: &str,
    expectation: &pc::RepositoryPublicationExpectation,
) -> Result<pc::RepositoryPublicationRejection, pc::SandboxError> {
    let rejection = match observation {
        "ABSENT\n" => pc::RepositoryPublicationRejection::RemoteRefAbsent,
        value => pc::RepositoryPublicationRejection::RemoteRefChanged {
            observed_commit: value
                .strip_prefix("REF:")
                .and_then(|value| value.strip_suffix('\n'))
                .ok_or_else(|| {
                    pc::SandboxError::invalid(
                        "container Repository conflict has no exact remote observation",
                    )
                })?
                .to_owned(),
        },
    };
    rejection.verify(expectation)?;
    Ok(rejection)
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
                "observe_remote() { remote_refs=; for attempt in 1 2 3; do ",
                "if remote_refs=$(git -c credential.useHttpPath=true -c \"credential.helper=$credential_helper\" ls-remote --refs \"$remote\" \"refs/heads/$branch\" 2>/dev/null); then break; fi; ",
                "if test \"$attempt\" = 3; then return 1; fi; sleep 1; done; ",
                "test \"$(printf '%s\\n' \"$remote_refs\" | awk 'END { print NR }')\" -eq 1 || return 1; ",
                "remote_commit=$(printf '%s\\n' \"$remote_refs\" | awk -v expected=\"refs/heads/$branch\" 'NF == 2 && $2 == expected { print $1 }'); ",
                "if test -n \"$remote_refs\" && test -z \"$remote_commit\"; then return 1; fi; }; ",
                "reject_stale() { if test -z \"$remote_commit\"; then printf 'ABSENT\\n'; exit 42; fi; ",
                "test \"${#remote_commit}\" -eq 40 || exit 43; ",
                "case \"$remote_commit\" in *[!0123456789abcdef]*) exit 43 ;; esac; ",
                "printf 'REF:%s\\n' \"$remote_commit\"; exit 42; }; ",
                "observe_remote || exit 43; ",
                "if test \"$remote_commit\" = \"$commit\"; then exit 0; fi; ",
                "if test \"$remote_commit\" != \"$prior\"; then reject_stale; fi; ",
                "before=$remote_commit; ",
                "if git -c credential.useHttpPath=true -c \"credential.helper=$credential_helper\" -C \"$repo\" push --porcelain --force-with-lease=\"refs/heads/$branch:$prior\" \"$remote\" \"$commit:refs/heads/$branch\" >/dev/null 2>&1; then exit 0; fi; ",
                "observe_remote || exit 43; ",
                "if test \"$remote_commit\" = \"$commit\"; then exit 0; fi; ",
                "if test \"$remote_commit\" != \"$before\"; then reject_stale; fi; ",
                "exit 43"
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
        stdio: pc::Stdio::Piped,
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
        assert_eq!(command.stdio, pc::Stdio::Piped);
        assert!(script.contains("printf 'REF:%s"));
        assert!(script.contains("push --porcelain"));
        assert!(script.contains(">/dev/null 2>&1"));
        assert_eq!(command.argv[9], "/opt/custom/awaken-sandbox");
        assert!(!format!("{command:?}").contains("password"));
    }

    #[test]
    fn container_conflict_channel_accepts_only_verified_remote_ref_evidence() {
        // Causes: C1 exact 40-hex third ref, C2 absent ref with update prior,
        // C3 desired/prior/malformed or untrusted channel text. Effects: E1/E2
        // reuse the existing permanent rejection contract; E3 remains
        // unavailable and can never close the terminal Session as rejected.
        // | Rule | observation | expectation | effect |
        // | P1 | third ref | create/update | E1 |
        // | P2 | absent | update | E2 |
        // | P3 | desired/prior/invalid | any | E3 |
        let desired = "1".repeat(40);
        let prior = "2".repeat(40);
        let third = "3".repeat(40);
        let create = pc::RepositoryPublicationExpectation {
            branch: "awf/work".into(),
            commit: desired.clone(),
            expected_prior_commit: None,
        };
        let update = pc::RepositoryPublicationExpectation {
            expected_prior_commit: Some(prior.clone()),
            ..create.clone()
        };
        assert_eq!(
            decode_publication_rejection(&format!("REF:{third}\n"), &create).unwrap(),
            pc::RepositoryPublicationRejection::RemoteRefChanged {
                observed_commit: third.clone()
            },
            "P1"
        );
        assert_eq!(
            decode_publication_rejection("ABSENT\n", &update).unwrap(),
            pc::RepositoryPublicationRejection::RemoteRefAbsent,
            "P2"
        );
        for (wire, expectation) in [
            (format!("REF:{desired}\n"), &create),
            (format!("REF:{prior}\n"), &update),
            ("ABSENT\n".into(), &create),
            ("REF:short\n".into(), &update),
            ("REF:3\nBearer token".into(), &update),
            (String::new(), &update),
        ] {
            assert!(
                decode_publication_rejection(&wire, expectation).is_err(),
                "P3"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn push_observation_failure_cannot_masquerade_as_an_absent_remote_ref() {
        use std::os::unix::fs::PermissionsExt as _;

        // Cause/effect table: R1 transient read then prior -> one leased push;
        // R2 every read fails -> unavailable (43); R3 third commit -> bounded
        // exact ref evidence (42); R4 absent create lease -> one create; R5
        // already desired -> replay; R6 failed push then desired -> response-
        // loss success; R7 failed push then third -> exact rejection evidence;
        // R8 failed push with unchanged prior -> unavailable; R9 absent update
        // prior -> typed absent evidence; R10 malformed OID and R11 multiple
        // ref lines -> unavailable.
        // Git diagnostic output is suppressed; only the bounded observation
        // marker may cross the Sandbox channel.
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
    sha=$FAKE_REMOTE_SHA
    if test -e "$FAKE_PUSH_FILE" && test -n "$FAKE_AFTER_SHA"; then sha=$FAKE_AFTER_SHA; fi
    if test -n "$sha"; then printf '%s\trefs/heads/awf/work\n' "$sha"; fi
    exit 0 ;;
  *" push "*) : > "$FAKE_PUSH_FILE"; exit "${FAKE_PUSH_CODE:-0}" ;;
esac
exit 99
"#,
        )
        .unwrap();
        std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).unwrap();
        let desired = "1".repeat(40);
        let prior = "2".repeat(40);
        let third = "3".repeat(40);
        let malformed = format!("{}X", "3".repeat(39));
        let multiple = format!("{third}\n{prior}");
        for (rule, failures, remote, after, expected_prior, push_code, code, pushes, reads) in [
            (
                "R1",
                1,
                prior.as_str(),
                "",
                Some(prior.as_str()),
                0,
                0,
                true,
                2,
            ),
            (
                "R2",
                3,
                prior.as_str(),
                "",
                Some(prior.as_str()),
                0,
                43,
                false,
                3,
            ),
            (
                "R3",
                0,
                third.as_str(),
                "",
                Some(prior.as_str()),
                0,
                42,
                false,
                1,
            ),
            ("R4", 0, "", "", None, 0, 0, true, 1),
            (
                "R5",
                0,
                desired.as_str(),
                "",
                Some(prior.as_str()),
                0,
                0,
                false,
                1,
            ),
            (
                "R6",
                0,
                prior.as_str(),
                desired.as_str(),
                Some(prior.as_str()),
                128,
                0,
                true,
                2,
            ),
            (
                "R7",
                0,
                prior.as_str(),
                third.as_str(),
                Some(prior.as_str()),
                128,
                42,
                true,
                2,
            ),
            (
                "R8",
                0,
                prior.as_str(),
                prior.as_str(),
                Some(prior.as_str()),
                128,
                43,
                true,
                2,
            ),
            ("R9", 0, "", "", Some(prior.as_str()), 0, 42, false, 1),
            (
                "R10",
                0,
                malformed.as_str(),
                "",
                Some(prior.as_str()),
                0,
                43,
                false,
                1,
            ),
            (
                "R11",
                0,
                multiple.as_str(),
                "",
                Some(prior.as_str()),
                0,
                43,
                false,
                1,
            ),
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
                .env("FAKE_AFTER_SHA", after)
                .env("FAKE_PUSH_CODE", push_code.to_string())
                .env("FAKE_FAILS", failures.to_string())
                .env("FAKE_COUNT_FILE", &count_file)
                .env("FAKE_PUSH_FILE", &push_file)
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(code), "{rule}: {output:?}");
            let expected_output = match rule {
                "R3" | "R7" => format!("REF:{third}\n"),
                "R9" => "ABSENT\n".to_owned(),
                _ => String::new(),
            };
            assert_eq!(
                output.stdout,
                expected_output.as_bytes(),
                "{rule}: safe wire"
            );
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
